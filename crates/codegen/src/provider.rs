/// Builds a provider trait and its managed implementation adapter for one variant.
#[derive(Default)]
pub(crate) struct Provider {
    declarations: String,
    installations: String,
    state_helpers: String,
}

impl Provider {
    pub fn method(
        &mut self,
        wire: &str,
        ident: &str,
        args: Option<&str>,
        result: Option<&str>,
        native: bool,
    ) {
        let parameter = args
            .map(|kind| format!("args: super::{kind}, "))
            .unwrap_or_default();
        let result_kind = result
            .map(|kind| format!("super::{kind}"))
            .unwrap_or_else(|| "()".into());
        self.declarations.push_str(&format!(
            r#"
            fn {ident}(&self, {parameter}context: ::tembra::Context)
                -> impl ::std::future::Future<Output = ::tembra::Result<{result_kind}>>;
        "#
        ));
        let arguments = if args.is_some() { "args, " } else { "" };
        let invocation =
            format!("provider.{ident}({arguments}::tembra::Context::from_core(context)).await");
        if native {
            let args_kind = args
                .map(|kind| format!("super::{kind}"))
                .unwrap_or_else(|| "()".into());
            let pattern = if args.is_some() { "args" } else { "_args" };
            self.installations.push_str(&format!(
                r#"
                let captured = provider.clone();
                let implementation = implementation.method(
                    {wire:?},
                    move |{pattern}: {args_kind}, context| {{
                        let provider = captured.clone();
                        ::std::boxed::Box::pin(async move {{ {invocation} }})
                    }}
                );
            "#
            ));
        } else {
            let decode = args
                .map(|kind| {
                    format!(
                        r#"
                let args: super::{kind} = ::tembra::serde_json::from_value(
                    args.ok_or_else(|| ::tembra::RuntimeError::invalid(
                        "missing_arguments", "method requires arguments"
                    ))?
                ).map_err(|error| ::tembra::RuntimeError::invalid(
                    "argument_conversion", error.to_string()
                ))?;
            "#
                    )
                })
                .unwrap_or_else(|| "let _ = args;".into());
            let encode = if result.is_some() {
                r#"::tembra::contracts::to_portable_value(&result).map(Some).map_err(|error|
                    ::tembra::RuntimeError::invalid(error.reason, error.message))"#
            } else {
                "let () = result; Ok(None)"
            };
            self.installations.push_str(&format!(
                r#"
                let captured = provider.clone();
                let implementation = implementation.json_method({wire:?}, move |args, context| {{
                    let provider = captured.clone();
                    ::std::boxed::Box::pin(async move {{
                        {decode}
                        let result = {invocation}?;
                        {encode}
                    }})
                }});
            "#
            ));
        }
    }

    pub fn state(&mut self, wire: &str, ident: &str, kind: &str) {
        self.declarations
            .push_str(&format!("fn {ident}(&self) -> ::tembra::Readable<{kind}>;"));
        self.installations.push_str(&format!(
            "let implementation = implementation.state({wire:?}, provider.{ident}().core()?);"
        ));
        let ident = ident.trim_start_matches("r#");
        self.state_helpers.push_str(&format!(
            r#"
            /// Builds typed state using this member's exact canonical contract schema.
            /// The declaration's internal cell name is chosen separately by the factory.
            ///
            /// # Errors
            /// Refuses incompatible linked descriptors, nonportable or invalid initial values,
            /// and failed Rust conversions before creating a core state declaration.
            pub fn {ident}_state(initial: {kind})
                -> ::tembra::Result<::tembra::StateDeclaration<{kind}>>
            {{
                ::tembra::StateDeclaration::with_schema(Self::state_schema({wire:?})?, initial)
            }}

            /// Builds typed state initialized asynchronously under managed core activation.
            /// This is a state initializer; factories remain synchronous.
            ///
            /// # Errors
            /// Refuses incompatible linked descriptors and unsupported schemas now.
            /// Callback, conversion and validation errors later fail activation.
            pub fn {ident}_state_with_initializer<F, Fut>(initialize: F)
                -> ::tembra::Result<::tembra::StateDeclaration<{kind}>>
            where
                F: Fn(::tembra::Context) -> Fut + 'static,
                Fut: ::std::future::Future<Output = ::tembra::Result<{kind}>> + 'static,
            {{
                ::tembra::StateDeclaration::with_schema_initializer(
                    Self::state_schema({wire:?})?, initialize
                )
            }}

            /// Declares a typed synchronous projection using the exact canonical state schema.
            /// Internal projection failure refuses the complete source candidate before adoption.
            ///
            /// # Errors
            /// Refuses unsupported schemas, invalid source bindings, failed conversions and
            /// computation errors. External source changes remain independently authoritative.
            pub fn {ident}_derive<D, F>(
                declaration: &mut ::tembra::Declaration<'_>, sources: D, compute: F
            ) -> ::tembra::Result<::tembra::Readable<{kind}>>
            where
                D: ::tembra::Sources,
                F: Fn(D::Values) -> ::tembra::Result<{kind}> + 'static,
            {{
                declaration.derive_with_schema(sources, Self::state_schema({wire:?})?, compute)
            }}
        "#
        ));
    }

    pub fn opaque(&mut self, wire: &str, ident: &str, kind: &str) {
        self.declarations.push_str(&format!(
            "fn {ident}(&self) -> ::tembra::core::Resource<{kind}>;"
        ));
        self.installations.push_str(&format!(
            "let implementation = implementation.opaque({wire:?}, provider.{ident}());"
        ));
    }

    pub fn emit(&self, variant: &str) -> String {
        let declarations = &self.declarations;
        let installations = &self.installations;
        let helpers = if self.state_helpers.is_empty() {
            String::new()
        } else {
            format!(
                r#"
                impl Token {{
                    fn state_schema(name: &str) -> ::tembra::Result<::tembra::serde_json::Value> {{
                        let contract = super::contract()?;
                        match contract.members(VARIANT).and_then(|members| members.get(name)) {{
                            Some(::tembra::contracts::Member::State {{ schema }}) =>
                                Ok(schema.clone()),
                            _ => Err(::tembra::RuntimeError::invalid(
                                "generated_state", "linked descriptor has no generated state member"
                            )),
                        }}
                    }}
                    {}
                }}
            "#,
                self.state_helpers
            )
        };
        let select = if variant.is_empty() {
            String::new()
        } else {
            format!("let implementation = implementation.variant({variant:?});")
        };
        format!(
            r#"
            /// Required typed provider methods and managed state/resource sources.
            /// Factories remain synchronous; asynchronous setup belongs to managed activation.
            pub trait Provider {{ {declarations} }}

            {helpers}

            impl<P: Provider + 'static> ::tembra::ProviderContract<P> for Token {{
                fn implementation(&self, provider: P)
                    -> ::tembra::Result<::tembra::core::Implementation>
                {{
                    let provider = ::std::rc::Rc::new(provider);
                    let implementation = ::tembra::core::Implementation::new()
                        .retain_owner(provider.clone());
                    {installations}
                    {select}
                    Ok(implementation)
                }}
            }}
        "#
        )
    }
}
