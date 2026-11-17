//! What a native Orb consumes from other Orbs. `orb(..)` takes `Providers`, one
//! `tembra::Provider` per interface the module imports, so a host that leaves one out does not
//! compile, and the Orb requires each provider, with the signature of the interface it imports,
//! as the plan of a compiled Orb requires the provider an installation selects. An interface of
//! types only needs no provider, as `cargo tembra build` binds a component's import of one to
//! nothing: the bindings' module serves its types. A function of an imported interface calls the
//! provider through the kernel, as a component's import does: the Orb module names the
//! interface's module, `gauge`, in place of the bindings' import module, and each function writes
//! its arguments as the boundary's JSON, issues the call with `issue_portable` under the context of
//! the method running, and reads the result back. A call the method stops awaiting is cancelled,
//! and the method ends once it has, as a component's executor cancels and joins an import its
//! callback abandons. A plain import reaches the provider's `main` object, through the reference
//! the Orb's create declares, as a component's root does; an import by key has its view's module,
//! whose `get` acquires the object of a key under the principal of the call, as a value whose
//! methods call that object, and whose claim the call that drops it releases as it settles, as a
//! component's view's is.
//!
//! A native method has no fiber of its own, which a component's call suspends while the kernel
//! completes an import: the functions of an imported interface are `async` natively, so an
//! interface whose functions are all `async func`, whose bindings are `async` in a component too,
//! is imported natively, and one with a synchronous function stays component-only.
//!
//! `Providers` also names the Orb that declares each hook the module contributes to: the Orb
//! requires the hook of it, with the signature of the hook's callback, and adds the contribution
//! with `Orb::contribute_portable`, whose callback reads the hook's input with the codec, runs the
//! author's function in the Orb's root, as a component's contribution runs, and writes its answer.
use super::{
    Kind, REGISTRY, Refused, Scopes, Uncovered, argument, boundary, decode, encode, kind, refused,
    spelled, wit_kind,
};
use crate::{Analysis, Contribution, hook_path, import_module, keyed, rust_ident, type_ident, wit};
use heck::ToSnakeCase;
use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::Ident;
use tembra_contracts::Contract;
use wit_parser::FunctionKind;

/// An interface the module imports from another Orb, which the native side serves.
pub(super) struct Import<'a> {
    /// The field of `Providers` naming its provider, and the module the Orb module names the
    /// import by: the interface's name, `gauge`, or its view's, `gauge_ref`, for an import by key.
    pub field: Ident,
    /// The interface the provider exports, `ns:name/interface@version`.
    pub interface: &'a str,
    /// The view of an import by key, which the world imports for it.
    pub view: Option<&'a keyed::View>,
    /// The signature of the interface, which the Orb requires of the provider's service.
    signature: String,
    /// The functions of the interface, which the Orb calls.
    methods: BTreeSet<String>,
}

impl Import<'_> {
    /// The identity of the interface the world imports: the interface itself, or its view.
    fn binding(&self, package: &str) -> String {
        match self.view {
            Some(view) => crate::identity(package, &view.name),
            None => self.interface.to_owned(),
        }
    }
}

/// The interfaces the module imports from other Orbs, which the native side serves, each with what
/// its provider must offer, leaving out an interface of types only, which needs none; or the
/// refusal of the first import it does not cover: a WASI import, an interface with a synchronous
/// function, which works in a component build only, or with a resource, or a function taking or
/// returning what the codec does not cover.
pub(super) fn imports(analysis: &Analysis) -> Result<Vec<Import<'_>>, Refused> {
    let mut imports = Vec::new();
    for identity in &analysis.imports {
        if crate::wasi::is_wasi(identity) {
            return Err(refused(&format!(
                "the import of {identity} is not supported natively yet"
            )));
        }
        served(analysis, identity, false)?;
        let interface = analysis
            .consumed_types
            .interface(identity)
            .expect("an imported interface is read");
        if interface.functions.is_empty() {
            continue;
        }
        let [_, _, field] = hook_path(identity);
        imports.push((field, identity.as_str(), None));
    }
    for view in &analysis.views {
        served(analysis, &view.target, true)?;
        imports.push((rust_ident(&view.name), view.target.as_str(), Some(view)));
    }
    if imports.is_empty() {
        return Ok(Vec::new());
    }
    // What each provider must offer, from the module's contract, as its plan requires it.
    let contract = contract(analysis)?;
    imports
        .into_iter()
        .map(|(field, interface, view)| {
            let mut import = Import {
                field,
                interface,
                view,
                signature: String::new(),
                methods: BTreeSet::new(),
            };
            let (_, signature, methods) = contract
                .import_requirement(&import.binding(&analysis.package), view.map(|_| interface))
                .map_err(unresolved)?;
            import.signature = signature;
            import.methods = methods;
            Ok(import)
        })
        .collect()
}

/// The module's contract: its WIT, with the consumed packages, as `cargo tembra build` and the
/// kernel read it.
fn contract(analysis: &Analysis) -> Result<Contract, Refused> {
    let file = format!("{}.wit", analysis.world);
    let world = wit::escaped(&analysis.world);
    match &analysis.wit_dir {
        Some(dir) => Contract::from_source_in(dir, &file, &analysis.wit, Some(&world)),
        None => Contract::from_source(&file, &analysis.wit, Some(&world)),
    }
    .map_err(unresolved)
}

/// The refusal of a contract the derived WIT does not resolve as.
fn unresolved(error: tembra_contracts::Error) -> Refused {
    refused(&format!(
        "the derived WIT does not resolve as a contract: {error}"
    ))
}

/// A hook the module contributes to, which the native side adds to the Orb.
pub(super) struct Contributed<'a> {
    /// The field of `Providers` naming the Orb that declares the hook, and the module the Orb
    /// module names the hook by, `guard`.
    pub field: Ident,
    pub contribution: &'a Contribution,
    /// The signature of the hook's callback, which the Orb requires of the hook.
    signature: String,
}

/// The hooks the module contributes to, each with the signature of its callback; or the refusal
/// of the first contribution taking or returning what the codec does not cover.
pub(super) fn contributions(analysis: &Analysis) -> Result<Vec<Contributed<'_>>, Refused> {
    for contribution in &analysis.contributions {
        let owner = &contribution.ident;
        let place = |role: &str, ty: &syn::Type, uncovered: Uncovered| {
            refused(&format!(
                "`{owner}` {role} `{}`{}",
                spelled(ty),
                uncovered.described(ty)
            ))
        };
        let (_, input) = &contribution.param;
        argument(input, contribution.scope, analysis)
            .map_err(|uncovered| place("takes", input, uncovered))?;
        if let Some(result) = &contribution.result {
            kind(result, contribution.scope, analysis)
                .map_err(|uncovered| place("returns", result, uncovered))?;
        }
    }
    if analysis.contributions.is_empty() {
        return Ok(Vec::new());
    }
    let contract = contract(analysis)?;
    analysis
        .contributions
        .iter()
        .map(|contribution| {
            let hook = &contribution.hook;
            let callback = &analysis.callbacks[hook];
            let [_, _, field] = hook_path(hook);
            Ok(Contributed {
                field,
                contribution,
                signature: contract
                    .function_signature(hook, &callback.name, true)
                    .map_err(unresolved)?,
            })
        })
        .collect()
}

/// The function adding a contribution to the Orb, `__tembra_contribution_admit` for `admit`.
pub(super) fn contribution_ident(contribution: &Contribution) -> Ident {
    format_ident!(
        "__tembra_contribution_{}",
        contribution.ident.to_string().to_snake_case()
    )
}

/// The function adding a contribution to the Orb, in the contribution's module, which `orb(..)`
/// calls with the Orb that declares the hook, `target`: `Orb::contribute_portable` with the hook,
/// the signature of its callback and the priority of the attribute. The callback reads the hook's
/// input with the codec, refusing it as the call's arguments, runs the author's function and
/// writes its answer as the plan composes it: none for a function without a result, the value of
/// an `Option` when it has one and none when it has none, and the value otherwise. It runs in the
/// Orb's root, as a component's contribution does: in the scope of the call, which the native
/// `context` reads, with the `context` option, of the Orb's root resources, with some, and of its
/// imports, `imported`, but on no object, so it reaches no state. The function is the registry's of
/// the contribution's module, which `orb(..)` reaches whatever the visibility of the module.
pub(super) fn contribution(
    contributed: &Contributed<'_>,
    analysis: &Analysis,
    imported: bool,
) -> TokenStream {
    let facade = &analysis.options.facade();
    let contribution = contributed.contribution;
    let scope = contribution.scope;
    let root = crate::supers(analysis.modules[scope].len());
    let boundary = boundary(scope, analysis);
    let scopes = Scopes {
        context: analysis
            .options
            .context
            .then(|| quote! { self::#root context::__scoped }),
        state: None,
        resources: (!analysis.resources.is_empty())
            .then(|| quote! { self::#root __tembra_resource::scoped }),
        imports: imported.then(|| quote! { self::#root __tembra_import::scoped }),
    };
    let ident = &contribution.ident;
    let (param, input) = &contribution.param;
    let read = decode(
        &argument(input, scope, analysis)
            .unwrap_or_else(|_| unreachable!("a contribution's input is covered")),
        quote! { value },
        &boundary,
        facade,
    );
    let answer = match &contribution.result {
        None => quote! {
            #ident(#param);
            ::core::result::Result::Ok(::core::option::Option::None)
        },
        Some(result) => {
            let written = match kind(result, scope, analysis)
                .unwrap_or_else(|_| unreachable!("a contribution's result is covered"))
            {
                Kind::Option(inner) => {
                    let item = encode(&inner, quote! { item }, &boundary, facade);
                    quote! {
                        match value {
                            ::core::option::Option::None => {
                                ::core::result::Result::Ok(::core::option::Option::None)
                            }
                            ::core::option::Option::Some(item) => {
                                #item.map(::core::option::Option::Some)
                            }
                        }
                    }
                }
                kind => {
                    let value = encode(&kind, quote! { value }, &boundary, facade);
                    quote! { #value.map(::core::option::Option::Some) }
                }
            };
            quote! {
                let value = #ident(#param);
                let write = |value: #result| -> ::core::result::Result<
                    ::core::option::Option<::#facade::serde_json::Value>,
                    ::std::string::String,
                > { #written };
                write(value).map_err(|message| {
                    ::#facade::Error::invalid(::#facade::core::Reason::Result, message)
                })
            }
        }
    };
    let future = scopes.around(quote! {
        async move {
            __tembra_context.check()?;
            let read = |value: ::#facade::serde_json::Value| -> ::core::result::Result<
                #input,
                ::std::string::String,
            > { #read };
            let #param = read(__tembra_input).map_err(|message| {
                ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message)
            })?;
            #answer
        }
    });
    let handle = scopes.handle(quote! {
        |__tembra_context: ::#facade::Context, __tembra_input: ::#facade::serde_json::Value| #future
    });
    let function = contribution_ident(contribution);
    let hook = &contribution.hook;
    let signature = &contributed.signature;
    let priority = contribution.priority;
    let resources = scopes.resources.is_some().then(|| {
        quote! { , resources: &::std::rc::Rc<self::#root __tembra_resource::Resources> }
    });
    let imports = imported.then(|| {
        quote! { , imports: &::std::rc::Rc<self::#root __tembra_import::Imports> }
    });
    let doc = format!(
        " The contribution of `{ident}` to the hook `{hook}`, which `orb(..)` adds to the native Orb, with the Orb that declares the hook, `target`."
    );
    let registry = format_ident!("{REGISTRY}");
    let registered = Literal::usize_unsuffixed(scope);
    quote! {
        impl #root #registry<#registered> {
            #[doc = #doc]
            pub(crate) fn #function(
                orb: ::#facade::Orb,
                target: &str
                #resources
                #imports
            ) -> ::#facade::Orb {
                orb.contribute_portable(target, #hook, #signature, #priority, #handle)
            }
        }
    }
}

/// Whether the native side serves the import of `identity`, by key or not: every function of the
/// interface is an `async func` taking and returning what the codec covers, and the interface has
/// no resource.
fn served(analysis: &Analysis, identity: &str, by_key: bool) -> Result<(), Refused> {
    let interface = analysis
        .consumed_types
        .interface(identity)
        .expect("an imported interface is read");
    let import = if by_key {
        format!("the import of {identity} by key")
    } else {
        format!("the import of {identity}")
    };
    if let Some(name) = interface
        .types
        .iter()
        .find(|(_, id)| {
            matches!(
                analysis
                    .consumed_types
                    .definition(wit_parser::Type::Id(**id)),
                Ok((_, wit_parser::TypeDefKind::Resource))
            )
        })
        .map(|(name, _)| name)
    {
        return Err(refused(&format!(
            "{import}, an interface with the resource `{name}`, is not supported natively yet"
        )));
    }
    for function in interface.functions.values() {
        let name = &function.name;
        if matches!(function.kind, FunctionKind::Freestanding) {
            return Err(refused(&format!(
                "{import} works in a component build only: its function `{name}` is synchronous, and natively a method has no fiber to suspend while the kernel completes the call, so a native Orb imports only interfaces whose functions are `async func`"
            )));
        }
        let types = function
            .params
            .iter()
            .map(|param| param.ty)
            .chain(function.result);
        for ty in types {
            if let Err(what) = wit_kind(ty, &analysis.consumed_types, 1) {
                return Err(refused(&format!(
                    "{import} is not supported natively yet: its function `{name}` takes or returns {what}"
                )));
            }
        }
    }
    Ok(())
}

/// `Providers`, the argument of `orb(..)` naming the Orb that answers each import, and the Orb
/// that declares each hook the module contributes to, as the facade named `facade` names them.
pub(super) fn providers(
    imports: &[Import<'_>],
    contributions: &[Contributed<'_>],
    facade: &Ident,
) -> TokenStream {
    let hooks = contributions.iter().map(|contributed| {
        let field = &contributed.field;
        let doc = format!(
            " The Orb that declares the hook `{}`, which this Orb contributes to.",
            contributed.contribution.hook
        );
        quote! {
            #[doc = #doc]
            pub #field: ::#facade::Provider,
        }
    });
    let fields = imports.iter().map(|import| {
        let field = &import.field;
        let doc = match import.view {
            Some(_) => format!(
                " The Orb whose objects answer the import of `{}` by key.",
                import.interface
            ),
            None => format!(
                " The Orb answering the import of `{}`, at its `main` object.",
                import.interface
            ),
        };
        quote! {
            #[doc = #doc]
            pub #field: ::#facade::Provider,
        }
    });
    quote! {
        /// The Orbs that answer what this Orb consumes from others, for `orb(..)`: one for each
        /// interface it imports and for each hook it contributes to, under the name of the module
        /// the Orb module names it by.
        pub struct Providers {
            #(#fields)*
            #(#hooks)*
        }
    }
}

/// `__tembra_providers`, in the Orb module: what the Orb requires of each provider, one
/// requirement per Orb, as the plan of a compiled Orb requires the providers an installation
/// selects: the signature of each interface imported from it and the functions of the interface,
/// and each hook it contributes to with the signature of the hook's callback. Two imports or hooks
/// from one Orb name one API version of it. The runtime is the facade named `facade`.
pub(super) fn requirements(
    imports: &[Import<'_>],
    contributions: &[Contributed<'_>],
    facade: &Ident,
) -> TokenStream {
    let hooks = contributions.iter().map(|contributed| {
        let field = &contributed.field;
        let hook = &contributed.contribution.hook;
        let signature = &contributed.signature;
        quote! {
            let requirement = entry(&mut requirements, &providers.#field)?;
            requirement.hooks.insert(::std::string::String::from(#hook));
            requirement.hook_signatures.insert(
                ::std::string::String::from(#hook),
                ::std::string::String::from(#signature),
            );
        }
    });
    let required = imports.iter().map(|import| {
        let field = &import.field;
        let interface = import.interface;
        let signature = &import.signature;
        let methods = &import.methods;
        quote! {
            let requirement = entry(&mut requirements, &providers.#field)?;
            let service = ::std::format!("{}::{}", providers.#field.name, #interface);
            requirement.signatures.insert(
                ::core::clone::Clone::clone(&service),
                ::std::string::String::from(#signature),
            );
            requirement
                .methods
                .entry(service)
                .or_default()
                .extend([#(#methods),*].map(::std::string::String::from));
        }
    });
    quote! {
        /// What this Orb requires of the Orbs that answer what it consumes.
        #[doc(hidden)]
        mod __tembra_providers {
            /// The requirement of each Orb `providers` names, as the kernel checks it when it
            /// installs the Orb.
            pub(crate) fn requirements(
                providers: &super::Providers,
            ) -> ::#facade::Result<::std::vec::Vec<::#facade::core::Requirement>> {
                let mut requirements = ::std::collections::BTreeMap::new();
                #({ #required })*
                #({ #hooks })*
                ::core::result::Result::Ok(requirements.into_values().collect())
            }

            /// The requirement of `provider`'s Orb, one per Orb, which names one API version.
            fn entry<'a>(
                requirements: &'a mut ::std::collections::BTreeMap<
                    ::std::string::String,
                    ::#facade::core::Requirement,
                >,
                provider: &::#facade::Provider,
            ) -> ::#facade::Result<&'a mut ::#facade::core::Requirement> {
                let requirement = requirements
                    .entry(::core::clone::Clone::clone(&provider.name))
                    .or_insert_with(|| ::#facade::core::Requirement {
                        signatures: ::core::default::Default::default(),
                        hook_signatures: ::core::default::Default::default(),
                        orb: ::core::clone::Clone::clone(&provider.name),
                        api_version: provider.api_version,
                        methods: ::core::default::Default::default(),
                        hooks: ::core::default::Default::default(),
                    });
                if requirement.api_version != provider.api_version {
                    return ::core::result::Result::Err(::#facade::Error::invalid(
                        ::#facade::core::Reason::ApiVersion,
                        ::std::format!("inconsistent API version for provider {}", provider.name),
                    ));
                }
                ::core::result::Result::Ok(requirement)
            }
        }
    }
}

/// `__tembra_import`, in the Orb module: the table of one Orb's imports, which `orb(..)` builds
/// from its `Providers` and whose plain imports the Orb's create declares, as a component's root
/// declares its references, and the call each method's future runs for, which the functions of
/// the import modules call with. A call that fails, or whose arguments or result the codec
/// refuses, ends the method that made it with the failure, as a trap ends a component's call. A
/// call the method stops awaiting is cancelled, and the method ends once it has ended, unless its
/// own call is cancelled, as a component's executor joins an import its callback abandons; then it
/// releases the claims of the views dropped in it. The runtime is the facade named `facade`.
pub(super) fn table(imports: &[Import<'_>], facade: &Ident) -> TokenStream {
    let (plain, keyed): (Vec<&Import<'_>>, Vec<&Import<'_>>) =
        imports.iter().partition(|import| import.view.is_none());
    let plain_fields: Vec<&Ident> = plain.iter().map(|import| &import.field).collect();
    let plain_interfaces = plain.iter().map(|import| import.interface);
    let keyed_fields: Vec<&Ident> = keyed.iter().map(|import| &import.field).collect();
    let keyed_interfaces = keyed.iter().map(|import| import.interface);
    let keyed_methods = keyed.iter().map(|import| {
        let methods = &import.methods;
        quote! { [#(#methods),*] }
    });
    let plain_items = (!plain.is_empty()).then(|| {
        quote! {
            impl Imports {
                /// Declare the reference of the Orb's root to the `main` object of each plain
                /// import's provider, in the Orb's create.
                pub(crate) fn declare(
                    &self,
                    declaration: &::#facade::Declaration,
                ) -> ::#facade::Result<()> {
                    #(self.#plain_fields.declare(declaration)?;)*
                    ::core::result::Result::Ok(())
                }
            }

            /// A plain import: the provider's service, and the reference of the Orb's root to its
            /// `main` object, once the create declared it.
            pub(crate) struct Plain {
                service: ::std::string::String,
                reference: ::core::cell::RefCell<
                    ::core::option::Option<::std::rc::Rc<::#facade::Reference>>,
                >,
            }

            impl Plain {
                fn new(service: ::std::string::String) -> Self {
                    Self {
                        service,
                        reference: ::core::cell::RefCell::new(::core::option::Option::None),
                    }
                }

                fn declare(&self, declaration: &::#facade::Declaration) -> ::#facade::Result<()> {
                    let reference = declaration.get(
                        &self.service,
                        ::std::vec![::std::string::String::from("main")],
                    )?;
                    self.reference.replace(::core::option::Option::Some(reference));
                    ::core::result::Result::Ok(())
                }

                /// The reference of the Orb's root to the provider's `main` object.
                pub(crate) fn reference(&self) -> ::std::rc::Rc<::#facade::Reference> {
                    self.reference
                        .borrow()
                        .clone()
                        .expect("the Orb's create declared the reference of its import")
                }
            }
        }
    });
    let keyed_items = (!keyed.is_empty()).then(|| {
        quote! {
            /// An import by key: the provider's service, and what the Orb requires of the
            /// objects it acquires.
            pub(crate) struct Keyed {
                service: ::std::string::String,
                options: ::#facade::AcquisitionOptions,
            }

            impl Keyed {
                fn new(service: ::std::string::String, api_version: u32, methods: &[&str]) -> Self {
                    Self {
                        service,
                        options: ::#facade::AcquisitionOptions {
                            api_version,
                            methods: methods
                                .iter()
                                .map(|method| ::std::string::String::from(*method))
                                .collect(),
                        },
                    }
                }

                /// Acquire the object of `key` for the call of `context`, under its principal, once
                /// the object is ready, as a component's view acquires it: the call's root files
                /// the claim, and a wait that fails, is cancelled or is dropped releases it at
                /// once. Cancelling the call ends the wait.
                pub(crate) async fn get(
                    &self,
                    context: &::#facade::Context,
                    key: ::std::vec::Vec<::std::string::String>,
                ) -> ::#facade::Result<Claim> {
                    let mut claim = Claim {
                        reference: context
                            .get_wait(&self.service, key, ::core::clone::Clone::clone(&self.options))
                            .await?,
                        held: false,
                    };
                    {
                        let signal = context.signal();
                        let mut ready = ::std::boxed::Box::pin(claim.reference.ready());
                        let mut cancelled = ::std::boxed::Box::pin(signal.cancelled());
                        ::core::future::poll_fn(|cx| {
                            if let ::core::task::Poll::Ready(ready) =
                                ::core::future::Future::poll(ready.as_mut(), cx)
                            {
                                return ::core::task::Poll::Ready(ready);
                            }
                            ::core::future::Future::poll(cancelled.as_mut(), cx)
                                .map(|()| signal.check())
                        })
                        .await?;
                    }
                    context.check()?;
                    claim.held = true;
                    ::core::result::Result::Ok(claim)
                }
            }

            /// The claim of an object an import by key acquired, which the call's root files.
            /// Dropped while `get` waits, it is released at once; held by a view, it is released
            /// when the call that drops the view settles, as a component releases its view's claim
            /// once the guest dropped the view, so the root keeps none after it.
            pub(crate) struct Claim {
                reference: ::std::rc::Rc<::#facade::Reference>,
                held: bool,
            }

            impl Claim {
                /// The reference of the claim, which calls the object.
                pub(crate) fn reference(&self) -> &::#facade::Reference {
                    &self.reference
                }
            }

            impl ::core::ops::Drop for Claim {
                fn drop(&mut self) {
                    if self.held {
                        let call = CALL.with(|current| current.borrow().clone());
                        if let ::core::option::Option::Some(call) = call {
                            call.dropped
                                .borrow_mut()
                                .push(::std::rc::Rc::clone(&self.reference));
                            return;
                        }
                    }
                    // A drop has no caller to answer: a release that fails goes unreported, as a
                    // component's does.
                    let _ = self.reference.release();
                }
            }
        }
    });
    quote! {
        /// What this Orb imports from other Orbs natively, and the call each method's future runs
        /// for, which the functions of the import modules call with.
        #[doc(hidden)]
        mod __tembra_import {
            /// The imports of one Orb `orb(..)` builds.
            pub(crate) struct Imports {
                #(pub(crate) #plain_fields: Plain,)*
                #(pub(crate) #keyed_fields: Keyed,)*
            }

            impl Imports {
                /// The imports of an Orb whose providers are `providers`.
                pub(crate) fn new(providers: &super::Providers) -> Self {
                    Self {
                        #(#plain_fields: Plain::new(::std::format!(
                            "{}::{}",
                            providers.#plain_fields.name,
                            #plain_interfaces
                        )),)*
                        #(#keyed_fields: Keyed::new(
                            ::std::format!("{}::{}", providers.#keyed_fields.name, #keyed_interfaces),
                            providers.#keyed_fields.api_version,
                            &#keyed_methods,
                        ),)*
                    }
                }
            }

            #plain_items
            #keyed_items

            ::std::thread_local! {
                static CALL: ::core::cell::RefCell<::core::option::Option<::std::rc::Rc<Call>>> =
                    const { ::core::cell::RefCell::new(::core::option::Option::None) };
            }

            /// The call of a method being polled: its context, the imports of its Orb, the
            /// failure of an import it made, which ends it, the calls of the imports it stopped
            /// awaiting, which it joins before it ends, and the claims of the views dropped in it,
            /// which it releases as it ends.
            pub(crate) struct Call {
                pub(crate) context: ::#facade::Context,
                pub(crate) imports: ::std::rc::Rc<Imports>,
                failed: ::core::cell::RefCell<::core::option::Option<::#facade::Error>>,
                abandoned: ::core::cell::RefCell<::std::vec::Vec<::#facade::IssuedCall>>,
                dropped: ::core::cell::RefCell<::std::vec::Vec<::std::rc::Rc<::#facade::Reference>>>,
            }

            /// Run a method's future for its call: while it is polled, the import modules call
            /// with that call, and an outer poll's call comes back after. A failed import ends
            /// the method with its failure. As a component's callback, the method ends once the
            /// calls of the imports it stopped awaiting have ended, and releases the claims of the
            /// views dropped in it.
            pub(crate) fn scoped<T, F>(
                context: ::#facade::Context,
                imports: ::std::rc::Rc<Imports>,
                future: F,
            ) -> impl ::core::future::Future<Output = ::#facade::Result<T>>
            where
                F: ::core::future::Future<Output = ::#facade::Result<T>>,
            {
                let call = ::std::rc::Rc::new(Call {
                    context,
                    imports,
                    failed: ::core::cell::RefCell::new(::core::option::Option::None),
                    abandoned: ::core::cell::RefCell::new(::std::vec::Vec::new()),
                    dropped: ::core::cell::RefCell::new(::std::vec::Vec::new()),
                });
                async move {
                    let mut future = ::std::boxed::Box::pin(future);
                    let result = ::core::future::poll_fn(|cx| {
                        let polled = {
                            let _entered = Entered::enter(::std::rc::Rc::clone(&call));
                            ::core::future::Future::poll(future.as_mut(), cx)
                        };
                        match call.failed.borrow_mut().take() {
                            ::core::option::Option::Some(error) => {
                                ::core::task::Poll::Ready(::core::result::Result::Err(error))
                            }
                            ::core::option::Option::None => polled,
                        }
                    })
                    .await;
                    // The method's future goes first, in its call, and the imports it still
                    // awaits and the views it still holds with it.
                    {
                        let _entered = Entered::enter(::std::rc::Rc::clone(&call));
                        ::core::mem::drop(future);
                    }
                    call.join().await;
                    for reference in call.dropped.take() {
                        // As for a component's view, a release that fails goes unreported.
                        let _ = reference.release();
                    }
                    result
                }
            }

            /// The call set for one poll, which puts the previous one back when dropped, also
            /// when the poll panics.
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
            pub(crate) fn current() -> ::std::rc::Rc<Call> {
                CALL.with(|current| current.borrow().clone())
                    .expect("an imported function is called inside a method of the Orb")
            }

            impl Call {
                /// Call `method` of the object `reference` holds with `arguments`, the boundary's
                /// JSON, and read its result with `read`; a failure ends the method making the
                /// call.
                pub(crate) async fn call<R>(
                    &self,
                    reference: &::#facade::Reference,
                    method: &str,
                    arguments: ::core::result::Result<
                        ::#facade::serde_json::Value,
                        ::std::string::String,
                    >,
                    read: impl ::core::ops::FnOnce(
                        ::#facade::serde_json::Value,
                    ) -> ::core::result::Result<R, ::std::string::String>,
                ) -> R {
                    let answered = match arguments {
                        ::core::result::Result::Ok(arguments) => {
                            match self.issue(reference, method, arguments).await {
                                ::core::result::Result::Ok(value) => {
                                    read(value).map_err(|message| {
                                        ::#facade::Error::invalid(
                                            ::#facade::core::Reason::Result,
                                            message,
                                        )
                                    })
                                }
                                ::core::result::Result::Err(error) => {
                                    ::core::result::Result::Err(error)
                                }
                            }
                        }
                        ::core::result::Result::Err(message) => ::core::result::Result::Err(
                            ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message),
                        ),
                    };
                    match answered {
                        ::core::result::Result::Ok(value) => value,
                        ::core::result::Result::Err(error) => {
                            self.failed.borrow_mut().get_or_insert(error);
                            ::core::future::pending().await
                        }
                    }
                }

                /// Issue the call of `method` with `arguments` to the object `reference` holds,
                /// and wait for its result. Dropped first, it cancels the call, as a component's
                /// executor cancels an import its callback abandons, and leaves it for the method
                /// to join.
                async fn issue(
                    &self,
                    reference: &::#facade::Reference,
                    method: &str,
                    arguments: ::#facade::serde_json::Value,
                ) -> ::#facade::Result<::#facade::serde_json::Value> {
                    let (issued, result) = reference
                        .issue_portable::<::#facade::serde_json::Value, ::#facade::serde_json::Value>(
                            &self.context,
                            method,
                            arguments,
                        )
                        .await?;
                    let mut awaited = Awaited {
                        call: self,
                        issued: ::core::option::Option::Some(issued),
                    };
                    let answered = result.await;
                    // Answered: nothing is left to cancel.
                    awaited.issued = ::core::option::Option::None;
                    answered
                }

                /// Wait until the calls of the imports the method stopped awaiting have ended, as
                /// a component's callback joins them before it settles. Once the method's own call
                /// is cancelled, it stops waiting: they stay cancelled, and counted until they end.
                async fn join(&self) {
                    let abandoned = self.abandoned.take();
                    // Only a method that abandoned a call observes its own signal.
                    if abandoned.is_empty() {
                        return;
                    }
                    let signal = self.context.signal();
                    for issued in abandoned {
                        let mut settled = ::std::boxed::Box::pin(issued.settled());
                        let mut cancelled = ::std::boxed::Box::pin(signal.cancelled());
                        let stopped = ::core::future::poll_fn(|cx| {
                            if ::core::future::Future::poll(settled.as_mut(), cx).is_ready() {
                                return ::core::task::Poll::Ready(false);
                            }
                            ::core::future::Future::poll(cancelled.as_mut(), cx).map(|()| true)
                        })
                        .await;
                        if stopped {
                            return;
                        }
                    }
                }
            }

            /// The call of an import whose result the method awaits: dropped before it, it
            /// cancels the call and leaves it to the method's call, which joins it.
            struct Awaited<'a> {
                call: &'a Call,
                issued: ::core::option::Option<::#facade::IssuedCall>,
            }

            impl ::core::ops::Drop for Awaited<'_> {
                fn drop(&mut self) {
                    if let ::core::option::Option::Some(issued) = self.issued.take() {
                        issued.cancel();
                        self.call.abandoned.borrow_mut().push(issued);
                    }
                }
            }
        }
    }
}

/// The module of each import, in the Orb module, in place of the bindings' import module, whose
/// types it re-exports: a function of a plain import calls the provider's `main` object, and the
/// view of an import by key has `get`, which acquires the object of a key, and the value it
/// returns, whose methods call that object. Each function is `async`, as the bindings of an
/// `async func` are, and takes its parameters owned.
pub(super) fn modules(imports: &[Import<'_>], analysis: &Analysis) -> TokenStream {
    let facade = &analysis.options.facade();
    let types = &analysis.consumed_types;
    let boundary = super::boundary_at(1);
    let modules = imports.iter().map(|import| {
        let field = &import.field;
        let interface = types
            .interface(import.interface)
            .expect("an imported interface is read");
        let bindings = import_module(1, &hook_path(&import.binding(&analysis.package)));
        let functions = interface.functions.values().map(|function| {
            let wit_name = &function.name;
            let name = rust_ident(wit_name);
            let docs = docs(function.docs.contents.as_deref());
            let params: Vec<Ident> = function
                .params
                .iter()
                .map(|param| rust_ident(&param.name))
                .collect();
            let spelled = function.params.iter().map(|param| types.owned(param.ty, 1));
            let encoded = function.params.iter().zip(&params).map(|(param, name)| {
                let kind = wit_kind(param.ty, types, 1).expect("a served import's types are covered");
                encode(&kind, quote! { #name }, &boundary, facade)
            });
            let (result, read) = match function.result {
                Some(ty) => {
                    let kind: Kind<'static> =
                        wit_kind(ty, types, 1).expect("a served import's types are covered");
                    (types.owned(ty, 1), decode(&kind, quote! { value }, &boundary, facade))
                }
                None => (quote! { () }, quote! { ::core::result::Result::Ok(()) }),
            };
            let (receiver, reference) = match import.view {
                Some(_) => (quote! { &self, }, quote! { self.claim.reference() }),
                None => (
                    quote! {},
                    quote! { &__tembra_call.imports.#field.reference() },
                ),
            };
            quote! {
                #(#[doc = #docs])*
                pub async fn #name(#receiver #(#params: #spelled),*) -> #result {
                    let __tembra_call = super::__tembra_import::current();
                    let __tembra_arguments = (|| -> ::core::result::Result<
                        ::#facade::serde_json::Value,
                        ::std::string::String,
                    > {
                        ::core::result::Result::Ok(::#facade::serde_json::Value::Array(
                            ::std::vec![#(#encoded?),*],
                        ))
                    })();
                    __tembra_call
                        .call(#reference, #wit_name, __tembra_arguments, |value| #read)
                        .await
                }
            }
        });
        let functions: Vec<TokenStream> = functions.collect();
        let module_doc = match import.view {
            Some(_) => format!(
                " The objects of `{}`, each selected by its key, natively: `get` acquires the object of a key under the principal of the call, and the methods of the value it returns call that object.",
                import.interface
            ),
            None => format!(
                " The interface `{}`, imported natively: each function calls the `main` object of the Orb `Providers` names for it, under the principal of the method's call.",
                import.interface
            ),
        };
        let body = match import.view {
            Some(_) => {
                let resource = type_ident(&interface.name.clone().unwrap_or_default());
                let target = import.interface;
                let doc = format!(" An object of `{target}`: its methods are the interface's functions. Dropping it releases its claim of the object when the call settles, as a component's view does.");
                quote! {
                    #[doc = #doc]
                    pub struct #resource {
                        claim: super::__tembra_import::Claim,
                    }

                    impl #resource {
                        #(#functions)*
                    }

                    /// The object of `key` under the principal of the call, or why it was refused.
                    pub async fn get(
                        key: ::std::vec::Vec<::std::string::String>,
                    ) -> ::core::result::Result<#resource, ::std::string::String> {
                        let __tembra_call = super::__tembra_import::current();
                        match __tembra_call
                            .imports
                            .#field
                            .get(&__tembra_call.context, key)
                            .await
                        {
                            ::core::result::Result::Ok(claim) => {
                                ::core::result::Result::Ok(#resource { claim })
                            }
                            ::core::result::Result::Err(error) => {
                                ::core::result::Result::Err(::std::string::ToString::to_string(&error))
                            }
                        }
                    }
                }
            }
            None => quote! { #(#functions)* },
        };
        quote! {
            #[doc = #module_doc]
            pub mod #field {
                #[allow(unused_imports)]
                pub use #bindings::*;

                #body
            }
        }
    });
    quote! { #(#modules)* }
}

/// A function's WIT documentation, as `#[doc]` lines.
fn docs(contents: Option<&str>) -> Vec<String> {
    contents
        .into_iter()
        .flat_map(str::lines)
        .map(|line| format!(" {line}"))
        .collect()
}
