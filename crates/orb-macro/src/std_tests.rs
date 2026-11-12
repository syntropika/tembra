//! Runtime ownership is explicit: std supplies its runtime unless `no_std` is selected.
use super::{Options, expand};

fn guest(no_std: bool) -> syn::ItemMod {
    syn::parse2(
        expand(
            Options {
                package: Some(syn::LitStr::new(
                    "example:runtime@1.0.0",
                    proc_macro2::Span::call_site(),
                )),
                no_std,
                ..Options::default()
            },
            syn::parse_quote! {
                mod guest {
                    #[service]
                    pub struct Greeting;
                    impl Greeting {
                        pub fn hello(name: String) -> String { name }
                    }
                }
            },
        )
        .expect("a valid guest"),
    )
    .expect("generated Rust")
}

#[test]
fn std_guest_does_not_define_an_allocator_or_panic_handler() {
    let module = guest(false);
    let tokens = quote::quote!(#module).to_string();
    assert!(!tokens.contains("global_allocator"));
    assert!(!tokens.contains("panic_handler"));
    assert!(!tokens.contains("dlmalloc"));
}

#[test]
fn freestanding_guest_supplies_a_wasm_only_runtime_without_changing_the_contract() {
    let std = guest(false);
    let freestanding = guest(true);
    let tokens = quote::quote!(#freestanding).to_string();
    assert!(tokens.contains("global_allocator"));
    assert!(tokens.contains("panic_handler"));
    let items = &freestanding.content.as_ref().expect("module body").1;
    for item in items {
        let attrs = match item {
            syn::Item::Static(item) if item.ident == "__TEMBRA_ALLOCATOR" => &item.attrs,
            syn::Item::Fn(item) if item.sig.ident == "__tembra_panic" => &item.attrs,
            _ => continue,
        };
        assert!(attrs.iter().any(|attr| {
            attr.path().is_ident("cfg") && quote::quote!(#attr).to_string().contains("wasm32")
        }));
    }
    let contract = |module: &syn::ItemMod| {
        module
            .content
            .as_ref()
            .unwrap()
            .1
            .iter()
            .find_map(|item| match item {
                syn::Item::Const(item) if item.ident == "WIT" => {
                    let expr = &item.expr;
                    Some(quote::quote!(#expr).to_string())
                }
                _ => None,
            })
            .expect("contract")
    };
    assert_eq!(contract(&std), contract(&freestanding));
}
