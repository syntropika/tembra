//! External DTO, contract-token, provider and consumer generation.
//! This crate is never a runtime dependency.

use std::{collections::BTreeSet, fmt};

use serde_json::Value;
use tembra_contracts::{Contract, Member, ValueSchema};
use typify::{TypeSpace, TypeSpaceSettings};

mod facade;
mod options;
mod presence;
mod provider;

pub use options::{GenerationOptions, NativeMethodBinding};

pub type Result<T> = std::result::Result<T, CodegenError>;

/// Observable reasons why a typed contract binding cannot be generated safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodegenErrorKind {
    Descriptor,
    NameCollision,
    UnsupportedName,
    UnmappedNative,
    InvalidBinding,
    UnrepresentablePresence,
    SchemaAdapter,
    UnsupportedDependency,
    Typify,
    GeneratedSyntax,
}

impl CodegenErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Descriptor => "descriptor",
            Self::NameCollision => "name_collision",
            Self::UnsupportedName => "unsupported_name",
            Self::UnmappedNative => "unmapped_native",
            Self::InvalidBinding => "invalid_binding",
            Self::UnrepresentablePresence => "unrepresentable_presence",
            Self::SchemaAdapter => "schema_adapter",
            Self::UnsupportedDependency => "unsupported_dependency",
            Self::Typify => "typify",
            Self::GeneratedSyntax => "generated_syntax",
        }
    }
}

/// A failed generation with a stable category, descriptor path and diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodegenError {
    kind: CodegenErrorKind,
    path: String,
    message: String,
}

impl CodegenError {
    pub fn kind(&self) -> CodegenErrorKind {
        self.kind
    }
    pub fn reason(&self) -> &'static str {
        self.kind.as_str()
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.reason(),
            self.path,
            self.message
        )
    }
}

impl std::error::Error for CodegenError {}

fn error(reason: &'static str, path: &str, message: impl Into<String>) -> CodegenError {
    let kind = match reason {
        "descriptor" => CodegenErrorKind::Descriptor,
        "name_collision" => CodegenErrorKind::NameCollision,
        "unsupported_name" => CodegenErrorKind::UnsupportedName,
        "unmapped_native" => CodegenErrorKind::UnmappedNative,
        "invalid_binding" => CodegenErrorKind::InvalidBinding,
        "unrepresentable_presence" => CodegenErrorKind::UnrepresentablePresence,
        "schema_adapter" => CodegenErrorKind::SchemaAdapter,
        "unsupported_dependency" => CodegenErrorKind::UnsupportedDependency,
        "typify" => CodegenErrorKind::Typify,
        "generated_syntax" => CodegenErrorKind::GeneratedSyntax,
        _ => unreachable!("unknown internal generator error category"),
    };
    CodegenError {
        kind,
        path: path.into(),
        message: message.into(),
    }
}

/// Generate DTOs, contract tokens and managed consumer references.
///
/// Every schema has its own module, so unrelated type names cannot shadow one another.
/// Native signatures require explicit capability mappings; use [`generate_rust_with_options`].
///
/// # Guarantees
///
/// Generation is deterministic and does not mutate the contract or write files.
/// DTOs preserve open object properties; acquisition defaults remain the core's
/// responsibility. Wrappers retain the embedded consumer token and invoke the
/// validated JSON boundary, distinguishing absent arguments/results from null.
/// Constructors require a live reference of their declared variant. State getters
/// wrap managed core readables. Generated code depends on the public `tembra` facade.
///
/// # Errors
///
/// Returns a typed [`CodegenError`] for unmapped native/opaque signatures, normalized
/// name collisions, unsupported names, or a failed Typify conversion. Optional
/// fields use generated `Presence<T>` values to preserve missing/null/empty data.
/// Synthesized union fields without an exact source mapping fail with
/// [`CodegenErrorKind::UnrepresentablePresence`]. This generator
/// supports a documented subset of valid descriptors, not every portable schema.
/// Failure has no contract or filesystem effects.
///
/// # Examples
///
/// ```
/// use std::collections::BTreeMap;
/// use serde_json::json;
/// use tembra_codegen::{generate_rust, CodegenErrorKind};
/// use tembra_contracts::{Contract, Member, ValueSchema};
///
/// let empty = Contract::local("example.empty", 1,
///     json!({"type":"object","properties":{}}), BTreeMap::new())?;
/// let original = empty.clone();
/// let source = generate_rust(&empty)?;
/// assert!(source.contains("DESCRIPTOR_JSON"));
/// assert_eq!(source, generate_rust(&empty)?);
/// assert_eq!(empty, original);
///
/// let native = Contract::local("example.native", 1,
///     json!({"type":"object","properties":{}}), BTreeMap::from([
///         ("run".into(), Member::Method {
///             args: Some(ValueSchema::Native("resource@1".into())), result: None,
///         }),
///     ]))?;
/// let before = native.clone();
/// assert_eq!(generate_rust(&native).unwrap_err().kind(), CodegenErrorKind::UnmappedNative);
/// assert_eq!(native, before);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn generate_rust(contract: &Contract) -> Result<String> {
    generate_rust_with_options(contract, &GenerationOptions::default())
}

/// Generate consumers with explicit local native bindings and an optional token name.
///
/// # Guarantees
/// Native methods dispatch through a managed reference without serialization. Remote
/// methods always retain the core's portable validation, including same-host calls.
/// Bindings do not modify descriptor identity or add portable capabilities. Each
/// variant exposes its own `Token`; single-variant contracts also export the token
/// at the root, named `GeneratedContract` unless overridden. Context-bound views
/// share the original claim and propagate the supplied context on every call.
/// Each state member adds `<member>_state` and `<member>_state_with_initializer`
/// associated constructors on its token. They retain the exact canonical state
/// schema without requiring generated DTOs to implement `JsonSchema`; install
/// their returned declarations through `tembra::Declaration::state_with`.
/// `<member>_derive` declares typed synchronous projections using that same schema.
/// Single-variant state accessors can forward loading declaration dependencies.
/// Single-variant methods delegate readiness and context cancellation to core dispatch.
/// Multi-variant accessors and methods require a live selected shape: using them
/// before that point requires an advanced core operation or a single-variant projection.
///
/// # Errors
/// Returns the same descriptor/schema errors as [`generate_rust`], or
/// [`CodegenErrorKind::InvalidBinding`] for invalid type expressions, nonexistent
/// methods, changed argument/result presence, or remote native overrides. No files
/// are written and the source contract is unchanged.
pub fn generate_rust_with_options(
    contract: &Contract,
    options: &GenerationOptions,
) -> Result<String> {
    options::validate(contract, options)?;
    let cache_name = private_cache_name(options, "__TEMBRA_DESCRIPTOR_CACHE");
    let slot_name = private_cache_name(options, "__tembra_descriptor_slot");
    let value_name = private_cache_name(options, "__tembra_cached_descriptor");
    let build_name = private_cache_name(options, "__tembra_build_descriptor");
    let error_name = private_cache_name(options, "__tembra_descriptor_error");
    let descriptor = serde_json::to_string(contract)
        .map_err(|cause| error("descriptor", "contract", cause.to_string()))?;
    let mut output = format!(
        r#"// Generated by tembra-codegen using Typify. Do not edit.
        pub const DESCRIPTOR_JSON: &str = {descriptor:?};
        pub const CONTRACT_FINGERPRINT: &str = {:?};

        /// Reuses this generated consumer's validated contract token on the current thread.
        /// Successful metadata is immutable and shared by every generated variant.
        ///
        /// # Errors
        /// Returns invalid/descriptor if the linked contract library refuses the descriptor.
        pub fn contract() -> ::tembra_core::Result<
            ::std::rc::Rc<::tembra_contracts::Contract>,
        > {{
            ::std::thread_local! {{
                static {cache_name}: ::std::cell::OnceCell<
                    ::std::rc::Rc<::tembra::contracts::Contract>,
                > = const {{ ::std::cell::OnceCell::new() }};
            }}
            let {build_name} = || {{
                ::tembra_contracts::Contract::from_json(DESCRIPTOR_JSON)
                    .map(::std::rc::Rc::new)
                    .map_err(|{error_name}| {{
                        ::tembra_core::RuntimeError::invalid(
                            "descriptor", {error_name}.to_string(),
                        )
                    }})
            }};
            {cache_name}.try_with(|{slot_name}| {{
                if let ::std::option::Option::Some({value_name}) = {slot_name}.get() {{
                    return ::std::result::Result::Ok({value_name}.clone());
                }}
                let {value_name} = {build_name}()?;
                ::std::result::Result::Ok(
                    {slot_name}.get_or_init(|| {value_name}).clone(),
                )
            }}).unwrap_or_else(|_| {build_name}())
        }}
        "#,
        contract.fingerprint()
    );
    output.push_str(presence::HELPER);
    output.push_str(&schema_module("input", contract.input(), "input")?);
    output.push_str("pub use input::Root as Input;\n");
    let mut variant_names = BTreeSet::new();
    let mut modules = BTreeSet::from(["input".to_owned()]);
    for (variant, members) in contract.variants() {
        let variant_name = if variant.is_empty() {
            "local".into()
        } else {
            normalized_name(variant, "variant")?
        };
        if !variant_names.insert(variant_name.clone()) {
            return Err(error(
                "name_collision",
                "variants",
                "variant names normalize to the same Rust module",
            ));
        }
        let variant_module = format!("variant_{variant_name}");
        if !modules.insert(variant_module.clone()) {
            return Err(error(
                "name_collision",
                variant,
                "duplicate generated module",
            ));
        }
        let mut methods = String::new();
        let mut provider = provider::Provider::default();
        let mut state_types = String::new();
        let mut member_names = BTreeSet::from([
            "new".to_owned(),
            "reference".to_owned(),
            "core".to_owned(),
            "clone".to_owned(),
            "with_context".to_owned(),
            "ensure_variant".to_owned(),
            "bind_token".to_owned(),
        ]);
        let mut state_names = Vec::new();
        let mut method_names = Vec::new();
        for (name, member) in members {
            let path = format!("variants/{variant}/{name}");
            let member_name = normalized_name(name, &path)?;
            if !member_names.insert(member_name.clone()) {
                return Err(error(
                    "name_collision",
                    &path,
                    "member names normalize to the same Rust identifier",
                ));
            }
            if matches!(member, Member::Method { .. })
                && !member_names.insert(format!("call_{member_name}"))
            {
                return Err(error(
                    "name_collision",
                    &path,
                    "legacy method alias collides with a member",
                ));
            }
            let method_ident = rust_method_ident(&member_name, &path)?;
            let base = format!("v_{variant_name}_m_{member_name}");
            match member {
                Member::Method { args, result } => {
                    method_names.push(name.clone());
                    let binding = options.native_methods.get(name);
                    let native = binding.is_some()
                        || [args, result]
                            .into_iter()
                            .any(|schema| matches!(schema, Some(ValueSchema::Native(_))));
                    let args_type = add_schema(
                        &mut output,
                        &mut modules,
                        &format!("{base}_args"),
                        args.as_ref(),
                        &format!("{path}/args"),
                        binding.and_then(|binding| binding.args.as_deref()),
                        options,
                    )?;
                    let result_type = add_schema(
                        &mut output,
                        &mut modules,
                        &format!("{base}_result"),
                        result.as_ref(),
                        &format!("{path}/result"),
                        binding.and_then(|binding| binding.result.as_deref()),
                        options,
                    )?;
                    provider.method(
                        name,
                        &method_ident,
                        args_type.as_deref(),
                        result_type.as_deref(),
                        native,
                    );
                    let parameter = args_type
                        .as_ref()
                        .map(|kind| format!(", args: super::{kind}"))
                        .unwrap_or_default();
                    let args_expression = if args_type.is_some() {
                        r#"Some(::tembra_contracts::to_portable_value(&args).map_err(|error| {
                            ::tembra_core::RuntimeError::invalid(
                                "argument_conversion",
                                error.to_string(),
                            )
                        })?)"#
                    } else {
                        "None"
                    };
                    let result_signature = result_type
                        .as_ref()
                        .map(|kind| format!("super::{kind}"))
                        .unwrap_or_else(|| "()".into());
                    let invoke = if native {
                        let arguments = if args_type.is_some() { "args" } else { "()" };
                        format!("self.reference.call({name:?}, {arguments}, ctx).await")
                    } else {
                        format!(
                            "let result = self.reference.call_json({name:?}, {args_expression}, ctx).await?; {decode}",
                            decode = if result_type.is_some() {
                                r#"let value = result.ok_or_else(|| {
                                ::tembra_core::RuntimeError::invalid("missing_result", "value-returning method returned no result")
                            })?;
                            ::serde_json::from_value(value).map_err(|error| {
                                ::tembra_core::RuntimeError::invalid("result_conversion", error.to_string())
                            })"#
                            } else {
                                "let _ = result; Ok(())"
                            }
                        )
                    };
                    let forward_args = if args_type.is_some() { "args, " } else { "" };
                    let channel = if native {
                        "checked native"
                    } else {
                        "validated portable"
                    };
                    let admission = if contract.variants().len() == 1 {
                        ""
                    } else {
                        "self.ensure_variant()?;"
                    };
                    methods.push_str(&format!(
                        r#"
                        /// Invokes the declared method through the core {channel} boundary.
                        ///
                        /// Argument conversion completes before dispatch. Cancellation follows the
                        /// supplied context; it does not roll back effects already performed.
                        ///
                        /// # Errors
                        /// Returns core admission, validation, cancellation or callback errors,
                        /// or DTO conversion errors. Result validation and conversion may fail
                        /// after the callback has performed application effects.
                        pub async fn call_{member_name}(
                            &self{parameter},
                            ctx: Option<&::tembra_core::Context>,
                        ) -> ::tembra_core::Result<{result_signature}> {{
                            {admission}
                            let ctx = ctx.or(self.context.as_ref());
                            {invoke}
                        }}

                        /// Invokes the declared method under this view's context, if supplied.
                        ///
                        /// # Errors
                        /// Returns checked admission, validation, conversion, callback or
                        /// cancellation errors. Application effects are not rolled back.
                        pub async fn {method_ident}(&self{parameter}) -> ::tembra_core::Result<{result_signature}> {{
                            self.call_{member_name}({forward_args}self.context.as_ref()).await
                        }}
                        "#
                    ));
                }
                Member::State { schema } => {
                    state_names.push(name.clone());
                    let module = format!("{base}_state");
                    if !modules.insert(module.clone()) {
                        return Err(error(
                            "name_collision",
                            &path,
                            "duplicate generated schema module",
                        ));
                    }
                    output.push_str(&schema_module(&module, schema, &path)?);
                    let state_name = member_name
                        .split('_')
                        .map(|part| {
                            let mut chars = part.chars();
                            chars
                                .next()
                                .map(|first| {
                                    first.to_ascii_uppercase().to_string() + chars.as_str()
                                })
                                .unwrap_or_default()
                        })
                        .collect::<String>();
                    state_types.push_str(&format!(
                        "pub use super::{module}::Root as State{state_name};\n"
                    ));
                    provider.state(name, &method_ident, &format!("State{state_name}"));
                    let binding = if contract.variants().len() == 1 {
                        format!("self.reference.readable({name:?})")
                    } else {
                        format!(
                            "self.ensure_variant().and_then(|()| self.reference.readable({name:?}))"
                        )
                    };
                    let admission = if contract.variants().len() == 1 {
                        "A loading dependency can be forwarded without reading its pending value."
                    } else {
                        "A live selected shape is required; call this accessor again after readiness."
                    };
                    methods.push_str(&format!(r#"
                        /// Returns a typed view of the managed state; this does not acquire a claim.
                        /// {admission}
                        pub fn {method_ident}(&self) -> ::tembra::Readable<State{state_name}> {{
                            ::tembra::Readable::from_result({binding})
                        }}
                    "#));
                }
                Member::Opaque { id } => {
                    let kind = options.native_types.get(id).ok_or_else(|| {
                        error(
                            "unmapped_native",
                            &path,
                            "opaque member requires an explicit Rust capability mapping",
                        )
                    })?;
                    let module = format!("{base}_opaque");
                    output.push_str(&format!("pub mod {module} {{ pub type Root = {kind}; }}"));
                    provider.opaque(name, &method_ident, &format!("super::{module}::Root"));
                    methods.push_str(&format!(r#"
                        /// Resolves the current managed opaque capability without serializing it.
                        ///
                        /// # Errors
                        /// Refuses ended claims, a different variant, or a native type mismatch.
                        pub fn {method_ident}(&self) -> ::tembra_core::Result<::std::rc::Rc<super::{module}::Root>> {{
                            self.ensure_variant()?;
                            self.reference.opaque({name:?})
                        }}
                    "#));
                }
            }
        }
        let states = literal_strings(&state_names);
        let method_keys = literal_strings(&method_names);
        let provider = provider.emit(variant);
        output.push_str(&format!(
            r#"
            pub mod {variant_module} {{
                pub const VARIANT: &str = {variant:?};
                pub const STATE_MEMBERS: &[&str] = &[{states}];
                pub const METHOD_MEMBERS: &[&str] = &[{method_keys}];
                {state_types}
                {provider}

                /// Contract marker selecting this generated consumer variant.
                #[derive(Clone, Copy, Debug, Default)]
                pub struct Token;

                impl ::tembra::ContractToken for Token {{
                    type Input = super::Input;
                    type Reference = Consumer;

                    fn descriptor(&self) -> ::tembra_core::Result<::std::rc::Rc<::tembra_contracts::Contract>> {{
                        super::contract()
                    }}

                    fn bind(&self, reference: ::tembra_core::Reference) -> ::tembra_core::Result<Consumer> {{
                        Consumer::bind_token(reference)
                    }}
                }}

                /// A checked typed view of one reference consumer token and selected variant.
                /// Clones share the same reference claim; they do not acquire new claims.
                #[derive(Clone)]
                pub struct Consumer {{
                    reference: ::tembra_core::Reference,
                    context: Option<::tembra_core::Context>,
                }}

                impl Consumer {{
                    /// Checks that the reference is live and matches this token and variant.
                    /// The reference is consumed on success and failure.
                    ///
                    /// # Errors
                    /// Returns an error for a different consumer token, a loading, released or
                    /// gone reference, or a different selected variant. No method is invoked.
                    pub fn new(
                        reference: ::tembra_core::Reference,
                    ) -> ::tembra_core::Result<Self> {{
                        let consumer = Self::bind_token(reference)?;
                        consumer.ensure_variant()?;
                        Ok(consumer)
                    }}

                    fn bind_token(reference: ::tembra_core::Reference) -> ::tembra_core::Result<Self> {{
                        if reference.contract().fingerprint() != super::CONTRACT_FINGERPRINT {{
                            return Err(::tembra_core::RuntimeError::new(
                                ::tembra_core::ErrorCode::ContractMismatch,
                                "consumer_contract",
                                "reference consumer token does not match generated contract",
                            ));
                        }}
                        let consumer = Self {{ reference, context: None }};
                        if consumer.reference.status() != ::tembra_core::RefStatus::Loading {{
                            consumer.ensure_variant()?;
                        }}
                        Ok(consumer)
                    }}

                    /// Borrows the underlying reference without acquiring a new claim.
                    pub fn reference(&self) -> &::tembra_core::Reference {{
                        &self.reference
                    }}

                    /// Borrows the managed reference without acquiring a new claim.
                    pub fn core(&self) -> &::tembra_core::Reference {{ &self.reference }}

                    /// Shares this claim and propagates the supplied root, principal and version.
                    /// Expired and foreign contexts are refused by the core when a call is made.
                    pub fn with_context(&self, context: &::tembra::Context) -> Self {{
                        Self {{ reference: self.reference.clone(), context: Some(context.core().clone()) }}
                    }}

                    fn ensure_variant(&self) -> ::tembra_core::Result<()> {{
                        match self.reference.status() {{
                            ::tembra_core::RefStatus::Loading => {{
                                return Err(::tembra_core::RuntimeError::new(
                                    ::tembra_core::ErrorCode::NotReady,
                                    "generated_reference",
                                    "generated consumer requires a live reference",
                                ));
                            }}
                            ::tembra_core::RefStatus::Released => {{
                                return Err(::tembra_core::RuntimeError::new(
                                    ::tembra_core::ErrorCode::Released,
                                    "released",
                                    "reference released",
                                ));
                            }}
                            ::tembra_core::RefStatus::Gone => {{
                                return Err(self.reference.error().unwrap_or_else(|| {{
                                    ::tembra_core::RuntimeError::new(
                                        ::tembra_core::ErrorCode::Gone,
                                        "gone",
                                        "reference is gone",
                                    )
                                }}));
                            }}
                            ::tembra_core::RefStatus::Live => {{}}
                        }}
                        let shape = self.reference.shape().ok_or_else(|| {{
                            ::tembra_core::RuntimeError::new(
                                ::tembra_core::ErrorCode::NotReady,
                                "shape",
                                "reference has no live shape",
                            )
                        }})?;
                        if shape.variant.as_deref().unwrap_or("") != VARIANT {{
                            return Err(::tembra_core::RuntimeError::new(
                                ::tembra_core::ErrorCode::NotInVariant,
                                "variant",
                                "reference variant does not match generated consumer",
                            ));
                        }}
                        Ok(())
                    }}

                    {methods}
                }}
            }}
            "#
        ));
        if contract.variants().len() == 1 {
            let name = options.name.as_deref().unwrap_or("GeneratedContract");
            output.push_str(&format!(
                "pub use {variant_module}::Token as {name};\n\
                 pub use {variant_module}::Consumer as {name}Reference;\n\
                 pub use {variant_module}::Provider as {name}Provider;\n\
                 pub use input::Root as {name}Input;\n"
            ));
        }
    }
    let mut parsed = syn::parse_file(&output)
        .map_err(|cause| error("generated_syntax", "output", cause.to_string()))?;
    check_items(&parsed.items, "output")?;
    facade::rewrite(&mut parsed)?;
    Ok(prettyplease::unparse(&parsed))
}

fn private_cache_name(options: &GenerationOptions, stem: &str) -> String {
    let mut name = stem.to_owned();
    while options
        .name
        .as_deref()
        .map(|value| value.trim_start_matches("r#"))
        == Some(name.as_str())
    {
        name.push('_');
    }
    name
}

fn add_schema(
    output: &mut String,
    modules: &mut BTreeSet<String>,
    module: &str,
    schema: Option<&ValueSchema>,
    path: &str,
    binding: Option<&str>,
    options: &GenerationOptions,
) -> Result<Option<String>> {
    let mapped = match schema {
        Some(ValueSchema::Native(id)) => Some(
            options
                .native_types
                .get(id)
                .map(String::as_str)
                .ok_or_else(|| {
                    error(
                        "unmapped_native",
                        path,
                        "native method signature requires an explicit Rust capability mapping",
                    )
                })?,
        ),
        _ => binding,
    };
    if let Some(kind) = mapped {
        if !modules.insert(module.into()) {
            return Err(error(
                "name_collision",
                path,
                "duplicate generated schema module",
            ));
        }
        output.push_str(&format!("pub mod {module} {{ pub type Root = {kind}; }}\n"));
        return Ok(Some(format!("{module}::Root")));
    }
    match schema {
        None => Ok(None),
        Some(ValueSchema::Native(_)) => Err(error(
            "unmapped_native",
            path,
            "native method signature requires an explicit Rust capability mapping",
        )),
        Some(ValueSchema::Portable(schema)) => {
            if !modules.insert(module.into()) {
                return Err(error(
                    "name_collision",
                    path,
                    "duplicate generated schema module",
                ));
            }
            output.push_str(&schema_module(module, schema, path)?);
            Ok(Some(format!("{module}::Root")))
        }
    }
}

fn rust_method_ident(value: &str, path: &str) -> Result<String> {
    if syn::parse_str::<syn::Ident>(value).is_ok() {
        return Ok(value.to_owned());
    }
    let raw = format!("r#{value}");
    syn::parse_str::<syn::Ident>(&raw)
        .map(|_| raw)
        .map_err(|cause| error("unsupported_name", path, cause.to_string()))
}

fn literal_strings(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn normalized_name(value: &str, path: &str) -> Result<String> {
    let mut result = String::new();
    let mut previous_lower = false;
    for character in value.chars() {
        if !character.is_ascii() {
            return Err(error(
                "unsupported_name",
                path,
                concat!(
                    "Rust API names must use ASCII characters; ",
                    "original wire names remain in the descriptor",
                ),
            ));
        }
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lower {
                result.push('_');
            }
            result.push(character.to_ascii_lowercase());
            previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            if !result.ends_with('_') {
                result.push('_');
            }
            previous_lower = false;
        }
    }
    let result = result.trim_matches('_').to_owned();
    if result.is_empty() {
        return Err(error(
            "unsupported_name",
            path,
            "name has no Rust identifier characters",
        ));
    }
    Ok(result)
}

fn schema_module(module: &str, schema: &Value, path: &str) -> Result<String> {
    let mut schema = schema.clone();
    strip_annotations(&mut schema);
    let mut adapter = presence::Adapter::default();
    adapter.prepare(&mut schema);
    let schema: schemars08::schema::Schema = serde_json::from_value(schema)
        .map_err(|cause| error("schema_adapter", path, cause.to_string()))?;
    let mut types = TypeSpace::new(&TypeSpaceSettings::default());
    let id = types
        .add_type_with_name(&schema, Some("Data".into()))
        .map_err(|cause| error("typify", path, cause.to_string()))?;
    adapter.add_property_types(&mut types, path)?;
    let root = types
        .get_type(&id)
        .map_err(|cause| error("typify", path, cause.to_string()))?
        .ident()
        .to_string();
    if types.uses_chrono() || types.uses_regress() || types.uses_uuid() {
        return Err(error(
            "unsupported_dependency",
            path,
            "DTO unexpectedly requires an optional external runtime dependency",
        ));
    }
    let body = types.to_stream().to_string();
    let mut parsed = syn::parse_file(&body)
        .map_err(|cause| error("generated_syntax", path, cause.to_string()))?;
    check_items(&parsed.items, path)?;
    adapter.rewrite(&mut parsed, path)?;
    if parsed
        .items
        .iter()
        .any(|item| item_ident(item).as_deref() == Some("Root"))
    {
        return Err(error(
            "name_collision",
            path,
            "DTO type collides with generated Root alias",
        ));
    }
    let body = prettyplease::unparse(&parsed);
    Ok(format!(
        "pub mod {module} {{ {body} pub type Root = {root}; }}\n"
    ))
}

fn strip_annotations(schema: &mut Value) {
    if let Some(object) = schema.as_object_mut() {
        // Metadata is retained in the embedded descriptor, never injected as Rust code or comments.
        for key in [
            "title",
            "description",
            "$comment",
            "examples",
            "readOnly",
            "writeOnly",
            "deprecated",
            "default",
        ] {
            object.remove(key);
        }
        if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
            for value in properties.values_mut() {
                strip_annotations(value);
            }
        }
        if object.get("type").and_then(Value::as_str) == Some("object")
            && object
                .get("additionalProperties")
                .is_none_or(|value| value == &Value::Bool(true))
        {
            // Typify otherwise discards valid open-object extras. A schema-valued
            // true policy is equivalent for DTO validation and emits a flatten map.
            object.insert("additionalProperties".into(), serde_json::json!({}));
        }
        if let Some(items) = object.get_mut("items") {
            strip_annotations(items);
        }
        if let Some(branches) = object.get_mut("anyOf").and_then(Value::as_array_mut) {
            for branch in branches {
                strip_annotations(branch);
            }
        }
    }
}

fn item_ident(item: &syn::Item) -> Option<String> {
    match item {
        syn::Item::Struct(item) => Some(item.ident.to_string()),
        syn::Item::Enum(item) => Some(item.ident.to_string()),
        syn::Item::Type(item) => Some(item.ident.to_string()),
        syn::Item::Mod(item) => Some(item.ident.to_string()),
        syn::Item::Trait(item) => Some(item.ident.to_string()),
        syn::Item::Const(item) => Some(item.ident.to_string()),
        syn::Item::Fn(item) => Some(item.sig.ident.to_string()),
        syn::Item::Use(item) => use_ident(&item.tree),
        _ => None,
    }
}

fn use_ident(tree: &syn::UseTree) -> Option<String> {
    match tree {
        syn::UseTree::Name(item) => Some(item.ident.to_string()),
        syn::UseTree::Rename(item) => Some(item.rename.to_string()),
        syn::UseTree::Path(item) => use_ident(&item.tree),
        _ => None,
    }
}

fn check_items(items: &[syn::Item], path: &str) -> Result<()> {
    let mut names = BTreeSet::new();
    for item in items {
        if let Some(name) = item_ident(item)
            && !names.insert(name)
        {
            return Err(error(
                "name_collision",
                path,
                "generated Rust items have duplicate names",
            ));
        }
        match item {
            syn::Item::Struct(item) => {
                let mut fields = BTreeSet::new();
                for field in &item.fields {
                    if let Some(ident) = &field.ident
                        && !fields.insert(ident.to_string())
                    {
                        return Err(error(
                            "name_collision",
                            path,
                            "DTO property names normalize to the same Rust field",
                        ));
                    }
                }
            }
            syn::Item::Enum(item) => {
                let mut variants = BTreeSet::new();
                for variant in &item.variants {
                    if !variants.insert(variant.ident.to_string()) {
                        return Err(error(
                            "name_collision",
                            path,
                            "enum values normalize to the same Rust variant",
                        ));
                    }
                }
            }
            syn::Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    check_items(items, path)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
