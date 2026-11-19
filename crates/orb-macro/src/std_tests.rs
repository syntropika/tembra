//! Runtime ownership is explicit: std supplies its runtime unless `no_std` is selected.
use super::{Options, expand, metadata::Metadata};

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
                    pub mod greeting {
                        #[service]
                        pub struct Greeting;
                        impl Greeting {
                            pub fn hello(name: String) -> String { name }
                        }
                    }
                }
            },
            Ok(Metadata::named("runtime")),
            None,
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
    let allocator = items
        .iter()
        .find_map(|item| match item {
            syn::Item::Static(item) if item.ident == "__TEMBRA_ALLOCATOR" => Some(item),
            _ => None,
        })
        .expect("the allocator");
    assert!(allocator.attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && quote::quote!(#attr).to_string().contains("wasm32")
    }));
    // The panic handler goes through the facade, which drops it when it links std, with `host`.
    let handler = items
        .iter()
        .find_map(|item| match item {
            syn::Item::Macro(item) => {
                let path = &item.mac.path;
                // For wasm32 only, so a native build never resolves the facade's path.
                let wasm = item.attrs.iter().any(|attr| {
                    attr.path().is_ident("cfg")
                        && quote::quote!(#attr).to_string().contains("wasm32")
                });
                (wasm && quote::quote!(#path).to_string() == ":: tembra :: __freestanding")
                    .then(|| syn::parse2::<syn::ItemFn>(item.mac.tokens.clone()).ok())
                    .flatten()
            }
            _ => None,
        })
        .expect("the panic handler, through the facade");
    assert_eq!(handler.sig.ident, "__tembra_panic");
    assert!(handler.attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && quote::quote!(#attr).to_string().contains("wasm32")
    }));
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

/// A refused attribute still names the facade its `crate = ".."` gives for the panic handler.
#[test]
fn a_refused_freestanding_guest_reaches_the_facade_by_its_name() {
    for (tail, facade) in [
        (quote::quote!(), ":: tembra :: __freestanding !"),
        (quote::quote!(, crate = "sdk"), ":: sdk :: __freestanding !"),
    ] {
        let refused = super::expansion(
            quote::quote!(package = "example:runtime@1.0.0", colour, no_std #tail),
            quote::quote!(
                mod guest {}
            ),
        )
        .to_string();
        assert!(refused.contains("unknown option"), "{refused}");
        assert!(refused.contains(facade), "{facade}\n{refused}");
    }
    // A `crate = ".."` that names no Rust identifier gives no facade: the handler is written as it
    // is, with no path the build would fail to resolve.
    let refused = super::expansion(
        quote::quote!(package = "example:runtime@1.0.0", crate = "my-sdk", no_std),
        quote::quote!(
            mod guest {}
        ),
    )
    .to_string();
    assert!(refused.contains("a Rust identifier"), "{refused}");
    assert!(!refused.contains("__freestanding"), "{refused}");
    assert_eq!(refused.matches("# [panic_handler]").count(), 1, "{refused}");
}
