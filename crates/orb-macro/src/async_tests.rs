//! Async forwarding preserves the fixed WIT rather than deriving a synchronous replacement.
use super::{Options, expand};
use std::path::PathBuf;

const CONTRACT: &str = "package example:events@1.0.0;
interface feed {
  resource updates {
    constructor();
    next: async func() -> option<u64>;
    fresh: static async func() -> updates;
  }
  subscribe: async func() -> updates;
  count: func() -> u32;
}
world events { import example:pace/clock@1.0.0; export feed; }
";
const IMPLEMENTATION: &str = "mod events {
    #[service(\"example:events/feed@1.0.0\")]
    pub struct Feed;
    impl Feed {
        pub async fn subscribe() -> Updates { Updates }
        pub fn count() -> u32 { 0 }
    }
    #[wit_resource(service = Feed)]
    pub struct Updates;
    impl Updates {
        #[constructor]
        pub fn new() -> Self { Self }
        pub async fn next(&self) -> Option<u64> { Some(1) }
        pub async fn fresh() -> Updates { Updates }
    }
}";

fn contract() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-wit");
    assert_eq!(
        std::fs::read_to_string(dir.join("events.wit")).unwrap(),
        CONTRACT
    );
    dir
}

fn expanded(source: &str) -> syn::Result<String> {
    let options = Options {
        package: Some(syn::LitStr::new(
            "example:consumer@1.0.0",
            proc_macro2::Span::call_site(),
        )),
        wit: Some(contract().display().to_string()),
        ..Options::default()
    };
    expand(options, syn::parse_str(source).unwrap()).map(|tokens| tokens.to_string())
}

#[test]
fn existing_wit_forwards_async_functions_and_resource_methods() {
    let generated = expanded(IMPLEMENTATION).unwrap();
    assert!(generated.contains("async fn subscribe"), "{generated}");
    assert!(
        generated.contains("Feed :: subscribe () . await"),
        "{generated}"
    );
    assert!(generated.contains("async fn next (& self"), "{generated}");
    assert!(
        generated.contains("Updates :: next (self ,) . await"),
        "{generated}"
    );
    assert!(
        generated.contains("Updates :: fresh () . await"),
        "{generated}"
    );
    assert!(
        generated.contains("fn count () -> u32 { Feed :: count () }"),
        "{generated}"
    );
}

#[test]
fn async_and_sync_author_functions_must_match_existing_wit() {
    for (before, after, message) in [
        (
            "pub async fn subscribe()",
            "pub fn subscribe()",
            "subscribe` must be `async fn`",
        ),
        (
            "pub async fn next(&self)",
            "pub fn next(&self)",
            "next` must be `async fn`",
        ),
        (
            "pub async fn fresh()",
            "pub fn fresh()",
            "fresh` must be `async fn`",
        ),
        (
            "pub fn count()",
            "pub async fn count()",
            "count` must be `fn`",
        ),
    ] {
        let error = expanded(&IMPLEMENTATION.replace(before, after))
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn derived_interfaces_keep_async_functions_and_resources() {
    let source = IMPLEMENTATION.replace("#[service(\"example:events/feed@1.0.0\")]", "#[service]");
    let generated = expanded(&source).unwrap();
    assert!(
        generated.contains("subscribe: async func() -> updates;"),
        "{generated}"
    );
    assert!(
        generated.contains("next: async func() -> option<u64>;"),
        "{generated}"
    );
    assert!(
        generated.contains("fresh: static async func() -> updates;"),
        "{generated}"
    );
    assert!(
        generated.contains("Updates :: next (self ,) . await"),
        "{generated}"
    );
}

#[test]
fn async_constructors_need_an_async_static_factory() {
    let error = expanded(&IMPLEMENTATION.replace("pub fn new()", "pub async fn new()"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("WIT constructors are synchronous"),
        "{error}"
    );
}
