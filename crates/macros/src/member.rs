use super::*;

pub(super) struct Members {
    pub builder: TokenStream,
    pub provider_methods: Vec<TokenStream>,
    pub reference_methods: Vec<TokenStream>,
    pub installations: Vec<TokenStream>,
    pub argument_types: Vec<TokenStream>,
}

pub(super) struct Scope<'a> {
    pub remote: bool,
    pub marker: &'a Ident,
    pub visibility: &'a syn::Visibility,
    pub version: &'a LitInt,
    pub receiver_local: &'a Ident,
    pub capture_local: &'a Ident,
}

pub(super) fn emit(member: &TraitItem, scope: &Scope<'_>, output: &mut Members) -> syn::Result<()> {
    let Scope {
        remote,
        marker,
        visibility,
        version,
        receiver_local,
        capture_local,
    } = *scope;
    let Members {
        builder,
        provider_methods,
        reference_methods,
        installations,
        argument_types,
    } = output;
    let validation::Parsed {
        method,
        member_name,
        docs,
        state,
        portable,
        opaque,
    } = validation::parse(member, remote)?;
    let signature = &method.sig;
    let ident = &signature.ident;
    if state || opaque.is_some() {
        if signature.asyncness.is_some()
            || signature.inputs.len() != 1
            || (state && opaque.is_some())
        {
            return ::std::result::Result::Err(syn::Error::new_spanned(
                signature,
                "state/opaque getters must be synchronous without arguments",
            ));
        }
        let ReturnType::Type(_, ty) = &signature.output else {
            return ::std::result::Result::Err(syn::Error::new_spanned(
                signature,
                "getter requires a value type",
            ));
        };
        if let ::std::option::Option::Some(id) = opaque {
            if remote {
                return ::std::result::Result::Err(syn::Error::new_spanned(
                    signature,
                    "remote contracts cannot expose opaque members",
                ));
            }
            *builder = quote!(#builder.opaque(#member_name, #id));
            provider_methods
                .push(quote!(#(#docs)* fn #ident(&self) -> ::tembra::core::Resource<#ty>;));
            reference_methods.push(quote! {
                #(#docs)*
                #visibility fn #ident(&self) -> ::tembra::Result<::std::rc::Rc<#ty>> {
                    self.raw.opaque(#member_name)
                }
            });
            installations.push(quote! {
                let implementation = implementation.opaque(#member_name, #receiver_local.#ident());
            });
        } else {
            *builder = quote!(#builder.typed_state::<#ty>(#member_name).map_err(__contract_error)?);
            provider_methods.push(quote!(#(#docs)* fn #ident(&self) -> ::tembra::Readable<#ty>;));
            reference_methods.push(
                quote!(#(#docs)* #visibility fn #ident(&self) -> ::tembra::Readable<#ty> {
                    ::tembra::Readable::from_reference(self.raw.clone(), #member_name)
                }),
            );
            installations.push(quote! {
                let implementation = implementation.state(
                    #member_name, #receiver_local.#ident().core()?,
                );
            });
        }
        return ::std::result::Result::Ok(());
    }
    if signature.asyncness.is_none() {
        return ::std::result::Result::Err(syn::Error::new_spanned(
            signature,
            "method members must be async",
        ));
    }
    let result = result_type(&signature.output)?;
    let void = matches!(result, Type::Tuple(tuple) if tuple.elems.is_empty());
    let mut arguments = Punctuated::<syn::PatType, Token![,]>::new();
    let mut names = Vec::new();
    let mut types = Vec::new();
    for argument in signature.inputs.iter().skip(1) {
        let FnArg::Typed(argument) = argument else {
            unreachable!()
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return ::std::result::Result::Err(syn::Error::new_spanned(
                argument,
                "arguments require simple names",
            ));
        };
        if pattern.ident == "ctx" {
            return ::std::result::Result::Err(syn::Error::new_spanned(
                pattern,
                "ctx is supplied to providers automatically",
            ));
        }
        arguments.push(argument.clone());
        names.push(pattern.ident.clone());
        types.push(argument.ty.as_ref().clone());
    }
    let (args_type, pack, unpack) = match names.len() {
        0 => (quote!(()), quote!(()), quote!(let () = args;)),
        1 => {
            let ty = &types[0];
            let arg = &names[0];
            (quote!(#ty), quote!(#arg), quote!(let #arg = args;))
        }
        _ if portable => {
            let args = format_ident!("{}{}Args", marker, pascal(&member_name));
            argument_types.push(quote! {
                #[derive(
                ::tembra::serde::Serialize, ::tembra::serde::Deserialize,
                ::tembra::schemars::JsonSchema,
            )]
                #[serde(crate = "::tembra::serde")]
                #[schemars(crate = "::tembra::schemars", description = "")]
                /// Portable argument object for this contract method.
            #visibility struct #args {
                #(
                        #[doc = "Typed value for this named method argument."]
                        #[schemars(description = "")]
                        pub #names: #types
                    ),*
            }
            });
            (
                quote!(#args),
                quote!(#args { #(#names),* }),
                quote!(let #args { #(#names),* } = args;),
            )
        }
        _ => (
            quote!((#(#types,)*)),
            quote!((#(#names,)*)),
            quote!(let (#(#names,)*) = args;),
        ),
    };
    let args_schema = if names.is_empty() {
        quote!(::std::option::Option::None)
    } else if portable {
        quote!(::std::option::Option::Some(::tembra::contracts::ValueSchema::Portable(
            ::tembra::contracts::schema_for::<#args_type>().map_err(__contract_error)?
        )))
    } else {
        native_schema(&args_type, version)
    };
    let result_schema = if void {
        quote!(::std::option::Option::None)
    } else if portable {
        quote!(::std::option::Option::Some(::tembra::contracts::ValueSchema::Portable(
            ::tembra::contracts::schema_for::<#result>().map_err(__contract_error)?
        )))
    } else {
        native_schema(&quote!(#result), version)
    };
    *builder = quote!(#builder.method(#member_name, #args_schema, #result_schema));
    provider_methods.push(quote! {
        #(#docs)*
        fn #ident(&self, #(#names: #types,)* ctx: ::tembra::Context)
            -> impl ::std::future::Future<Output = ::tembra::Result<#result>>;
    });
    let call = if portable {
        let encoded = if names.is_empty() {
            quote!(::std::option::Option::None)
        } else {
            quote!(::std::option::Option::Some(
                ::tembra::contracts::to_portable_value(&#pack).map_err(|error| {
                    ::tembra::RuntimeError::invalid("argument_conversion", error.message)
                })?
            ))
        };
        let decode = if void {
            quote!(let _ = value; ::std::result::Result::Ok(()))
        } else {
            quote!(
                ::tembra::contracts::from_portable_value(&value.ok_or_else(|| {
                    ::tembra::RuntimeError::invalid("missing_result", "missing method result")
                })?)
                .map_err(|error| ::tembra::RuntimeError::invalid(
                    "result_conversion",
                    error.to_string()
                ))
            )
        };
        quote! {
            let value = self.raw.call_json(#member_name, #encoded, self.context.as_ref()).await?;
            #decode
        }
    } else {
        quote!(self.raw.call::<#args_type, #result>(
            #member_name, #pack, self.context.as_ref(),
        ).await)
    };
    reference_methods.push(quote! {
        #(#docs)*
        #visibility async fn #ident(&self, #arguments) -> ::tembra::Result<#result> { #call }
    });
    let install = if portable {
        let decode = if names.is_empty() {
            quote!(let args = ();)
        } else {
            quote! {
                let args: #args_type = ::tembra::contracts::from_portable_value(&args.ok_or_else(|| {
                    ::tembra::RuntimeError::invalid("missing_arguments", "missing method arguments")
                })?).map_err(|error| {
                    ::tembra::RuntimeError::invalid("argument_conversion", error.to_string())
                })?;
            }
        };
        let encode = if void {
            quote!(let () = result; ::std::result::Result::Ok(::std::option::Option::None))
        } else {
            quote!(
                ::tembra::contracts::to_portable_value(&result)
                    .map(::std::option::Option::Some)
                    .map_err(|error| ::tembra::RuntimeError::invalid(error.reason, error.message))
            )
        };
        quote! {
            let #capture_local = #receiver_local.clone();
            let implementation = implementation.json_method(#member_name, move |args, ctx| {
                let #receiver_local = #capture_local.clone();
                ::std::boxed::Box::pin(async move {
                    #decode
                    #unpack
                    let result = #receiver_local.#ident(
                        #(#names,)* ::tembra::Context::from_core(ctx),
                    ).await?;
                    #encode
                })
            });
        }
    } else {
        quote! {
            let #capture_local = #receiver_local.clone();
            let implementation = implementation.method(#member_name, move |args: #args_type, ctx| {
                let #receiver_local = #capture_local.clone();
                ::std::boxed::Box::pin(async move {
                    #unpack
                    #receiver_local.#ident(#(#names,)* ::tembra::Context::from_core(ctx)).await
                })
            });
        }
    };
    installations.push(install);
    ::std::result::Result::Ok(())
}
