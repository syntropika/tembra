//! Declarative contracts that connect ordinary Rust providers to managed core references.

mod expand;

use proc_macro::TokenStream;

/// Declares a typed contract, provider trait, contract marker and managed reference wrapper.
///
/// Requires exactly one explicit `local` or `remote` scope; omission, repetition or contradiction
/// is a compile error. Local methods use native checked dispatch.
/// `#[portable]` selects portable local dispatch;
/// remote contracts always validate portable values. State remains owned by the core.
#[proc_macro_attribute]
pub fn contract(attributes: TokenStream, item: TokenStream) -> TokenStream {
    expand::contract(attributes.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
