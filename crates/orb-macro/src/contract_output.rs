//! Contract output selection at the Cargo authoring boundary.
use proc_macro2::{Span, TokenStream};
use quote::quote;
use std::path::{Path, PathBuf};
use syn::{Expr, Lit, LitStr};

/// An output path written literally or supplied by a Cargo build script.
pub(crate) enum Output {
    Literal(LitStr),
    Environment(LitStr),
}

/// Read a literal output path or the standard `env!("OUT_DIR")` spelling.
pub(crate) fn parse(expression: Expr) -> syn::Result<Output> {
    match expression {
        Expr::Lit(literal) => match literal.lit {
            Lit::Str(path) => Ok(Output::Literal(path)),
            _ => Err(syn::Error::new_spanned(
                literal,
                "`contract` needs a path string or env!(\"OUT_DIR\")",
            )),
        },
        Expr::Macro(expression) if expression.mac.path.is_ident("env") => {
            syn::parse2(expression.mac.tokens).map(Output::Environment)
        }
        other => Err(syn::Error::new_spanned(
            other,
            "`contract` needs a path string or env!(\"OUT_DIR\")",
        )),
    }
}

/// Prefer the build tool's explicit output, then the attribute, then the source default.
pub(crate) fn directory(output: Option<&Output>, manifest: &Path) -> syn::Result<PathBuf> {
    if let Some(path) = std::env::var_os("TEMBRA_CONTRACT_DIR") {
        if path.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                "TEMBRA_CONTRACT_DIR is empty",
            ));
        }
        return Ok(manifest.join(path));
    }
    let path = match output {
        Some(Output::Literal(path)) => PathBuf::from(path.value()),
        Some(Output::Environment(name)) => std::env::var_os(name.value())
            .map(PathBuf::from)
            .ok_or_else(|| syn::Error::new(name.span(), format!("contract output environment variable `{}` is not set; Cargo supplies OUT_DIR to packages with a build script", name.value())))?,
        None => PathBuf::from("contract"),
    };
    if path.as_os_str().is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "the contract output directory is empty",
        ));
    }
    Ok(manifest.join(path))
}

/// Make Cargo invalidate macro expansion when an output environment value changes.
pub(crate) fn tracking(output: Option<&Output>) -> TokenStream {
    let environment = match output {
        Some(Output::Environment(name)) => {
            Some(quote! { const _: ::core::option::Option<&str> = ::core::option_env!(#name); })
        }
        _ => None,
    };
    quote! {
        const _: ::core::option::Option<&str> = ::core::option_env!("TEMBRA_CONTRACT_DIR");
        #environment
    }
}
