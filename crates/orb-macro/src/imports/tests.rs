//! Public awaitability stays independent of the actual component ABI calling convention.
use crate::{Options, analyze, tests::literal};
use quote::quote;

#[test]
fn component_imports_wrap_sync_bindings_without_adding_an_await_or_a_yield() {
    let analysis = analyze(
        Options {
            package: Some(literal("example:consumer@1.0.0")),
            wit: Some(format!("{}/native-fixture/wit", env!("CARGO_MANIFEST_DIR"))),
            imports: vec![literal("example:protocol/codec@1.0.0")],
            ..Options::default()
        },
        syn::parse_quote! { mod consumer {
            #[service]
            pub struct Pipeline;
            impl Pipeline {
                pub async fn normalize(value: u64) -> u64 { codec::normalize(value).await }
            }
        } },
    )
    .unwrap();
    let generated = super::modules(&analysis, &Default::default());
    let source = syn::parse2::<syn::File>(generated).unwrap();
    let syn::Item::Mod(module) = &source.items[0] else {
        panic!("an import module")
    };
    let functions = &module.content.as_ref().unwrap().1;
    let normalize = functions
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "normalize" => Some(function),
            _ => None,
        })
        .unwrap();
    assert!(normalize.sig.asyncness.is_some());
    let body = &normalize.block;
    assert_eq!(
        quote!(#body).to_string(),
        "{ super :: example :: protocol :: codec :: normalize (value) }"
    );
    let generated = quote!(#source).to_string();
    assert!(generated.contains("pub async fn new"));
    assert!(generated.contains("pub async fn zero"));
    assert!(generated.contains("self . inner . next () . await"));
    assert!(generated.contains("value : __tembra_alloc :: string :: String"));
    assert!(!generated.contains("pub use super :: example :: protocol :: codec :: *"));
    assert!(!generated.contains("yield"));
}
