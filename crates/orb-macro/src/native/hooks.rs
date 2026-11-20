//! Native declarations bind the existing signed hook plans to their declaring object.
use super::{Kind, Refused, argument, boundary, decode, delivered_type, encode, refused, wit_kind};
use crate::{Analysis, Composition, Declared};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use tembra_contracts::Contract;

/// Emit descriptors, declaration-bound handles and the author's awaitable hook methods.
pub(super) fn emit(
    analysis: &Analysis,
    contract: Result<&Contract, &syn::Error>,
) -> Result<(TokenStream, Vec<(usize, TokenStream)>), Refused> {
    if analysis.declared.is_empty() {
        return Ok((TokenStream::new(), Vec::new()));
    }
    let contract = contract.map_err(|error| refused(&error.to_string()))?;
    let facade = analysis.options.facade();
    let mut descriptors = Vec::new();
    let mut fields = Vec::new();
    let mut types = Vec::new();
    let mut methods = Vec::new();
    for (index, declaration) in analysis.declared.iter().enumerate() {
        let field = format_ident!("hook_{index}");
        let name = &declaration.hook;
        let callback = declaration
            .input
            .as_ref()
            .map_or_else(|| analysis.callbacks[name].name.as_str(), |_| "apply");
        let signature = contract
            .function_signature(name, callback, true)
            .map_err(|error| refused(&error.to_string()))?;
        let (composition, result) = match declaration.composition {
            Composition::Series => (quote! { series }, quote! { () }),
            Composition::Waterfall => (
                quote! { waterfall },
                quote! { ::#facade::serde_json::Value },
            ),
            Composition::Bail => (
                quote! { bail },
                quote! { ::core::option::Option<::#facade::serde_json::Value> },
            ),
        };
        descriptors.push(quote! {
            /// Build the signed portable hook descriptor.
            fn #field() -> ::#facade::Hook<::#facade::serde_json::Value, #result> {
                ::#facade::Hook::#composition(#name).signature(#signature)
            }
        });
        methods.push((declaration.scope, method(declaration, &field, analysis)?));
        fields.push(field);
        types.push(result);
    }
    let message = "native hook declarations need the runtime's `hooks`: enable the facade's `hooks` feature in the host that installs this Orb";
    Ok((
        quote! {
            /// Signed hook descriptors and handles bound to one declaring scope.
            #[doc(hidden)]
            mod __tembra_hooks {
                ::#facade::__hooks! {
                    {
                        #(#descriptors)*

                        /// Register each hook with the existing runtime composition.
                        pub(crate) fn register(orb: ::#facade::Orb) -> ::#facade::Result<::#facade::Orb> {
                            ::core::result::Result::Ok(orb #(.hook(#fields()))*)
                        }

                        /// The hooks bound to the declaration of one service object or the Orb's root.
                        pub(crate) struct Hooks {
                            #(pub(crate) #fields: ::#facade::HookHandle<::#facade::serde_json::Value, #types>,)*
                        }

                        impl Hooks {
                            /// Bind handles while this object's declaration is open.
                            pub(crate) fn declare(declaration: &::#facade::Declaration) -> ::#facade::Result<Self> {
                                ::core::result::Result::Ok(Self {
                                    #(#fields: declaration.hook(&#fields())?,)*
                                })
                            }
                        }
                    }
                    {
                        /// Keep authoring builds valid when the installing host disables hooks.
                        pub(crate) struct Hooks;

                        /// Refuse installation before any declaration runs without hooks.
                        pub(crate) fn register(_orb: ::#facade::Orb) -> ::#facade::Result<::#facade::Orb> {
                            ::core::result::Result::Err(::#facade::Error::new(
                                ::#facade::core::Code::Unsupported, "native", #message,
                            ))
                        }

                        impl Hooks {
                            /// Refuse binding without the installing host's hooks feature.
                            pub(crate) fn declare(_declaration: &::#facade::Declaration) -> ::#facade::Result<Self> {
                                ::core::result::Result::Err(::#facade::Error::new(
                                    ::#facade::core::Code::Unsupported, "native", #message,
                                ))
                            }
                        }
                    }
                }
            }
        },
        methods,
    ))
}

/// Generate the shared domain-returning API, propagating transport failure through its callback.
fn method(
    declaration: &Declared,
    field: &syn::Ident,
    analysis: &Analysis,
) -> Result<TokenStream, Refused> {
    let facade = analysis.options.facade();
    let scope = declaration.scope;
    let depth = analysis.modules[scope].len();
    let root = crate::supers(depth);
    let boundary = boundary(scope, analysis);
    let (input, result) = if let Some((input, _)) = &declaration.input {
        let read =
            || argument(input, scope, analysis).map_err(|error| refused(&error.described(input)));
        let result = match declaration.composition {
            Composition::Series => None,
            Composition::Waterfall => Some(read()?),
            Composition::Bail => Some(Kind::Option(Box::new(read()?))),
        };
        (read()?, result)
    } else {
        let interface = analysis
            .consumed_types
            .interface(&declaration.hook)
            .expect("a consumed declared hook is read");
        let callback = interface
            .functions
            .values()
            .next()
            .expect("a hook has one callback");
        let read = |ty| {
            wit_kind(ty, &analysis.consumed_types, depth)
                .map_err(|_| refused("native hook declarations require portable data values"))
        };
        (
            read(callback.params[0].ty)?,
            callback.result.map(read).transpose()?,
        )
    };
    if input.resource().is_some()
        || result
            .as_ref()
            .is_some_and(|kind| kind.resource().is_some())
    {
        return Err(refused("native hook declarations cannot carry resources"));
    }
    let input_type = delivered_type(&input);
    let encoded = encode(&input, quote! { input }, &boundary, &facade);
    let (returned, decoded) = match &result {
        Some(kind) => {
            let returned = delivered_type(kind);
            let value = if matches!(declaration.composition, Composition::Bail) {
                quote! { answer.unwrap_or(::#facade::serde_json::Value::Null) }
            } else {
                quote! { answer }
            };
            let decoded = decode(kind, value, &boundary, &facade);
            (
                returned,
                quote! { #decoded.map_err(|message| ::#facade::Error::invalid(::#facade::core::Reason::Result, message)) },
            )
        }
        None => (
            quote! { () },
            quote! { let _ = answer; ::core::result::Result::Ok(()) },
        ),
    };
    let ident = &declaration.ident;
    Ok(quote! {
        impl #ident {
            /// Run the current caller's hook plan using this service object's declaration.
            pub async fn run(input: #input_type) -> #returned {
                ::#facade::__hooks! {
                    {
                        let call = self::#root __tembra_import::current();
                        let answered = async {
                            let encoded: ::core::result::Result<::#facade::serde_json::Value, ::std::string::String> = #encoded;
                            let input = encoded.map_err(|message| ::#facade::Error::invalid(::#facade::core::Reason::Arguments, message))?;
                            let answer = call.hooks.#field.run(&call.context, input).await?;
                            #decoded
                        }.await;
                        call.answer(answered).await
                    }
                    {
                        let _ = input;
                        self::#root __tembra_import::current().answer(
                            ::core::result::Result::Err(::#facade::Error::new(
                                ::#facade::core::Code::Unsupported,
                                "native",
                                "native hook declarations need the runtime's `hooks` feature",
                            )),
                        ).await
                    }
                }
            }
        }
    })
}
