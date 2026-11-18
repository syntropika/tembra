//! Keyed imports: the view the macro derives from the provider's WIT, the role it embeds, the
//! module it re-exports and the imports it refuses.
use super::tests::{
    expanded_with as expanded, literal, refused_with as refused, resolved_with, wit,
};
use super::{Options, expansion};
use std::path::{Path, PathBuf};
use syn::ItemMod;

const COUNTERS: &str = "package example:counters@1.0.0;

/// Counting objects.
interface counter {
  /// What an object has counted.
  record tally {
    key: list<string>,
    count: u32,
  }
  /// Add `amount` to the count.
  ///
  /// Return the new count.
  add: func(amount: u32) -> u32;
  read: async func() -> tally;
}
";

const ROUTER: &str = "mod router {
    #[service]
    pub struct Routing;
    impl Routing {
        pub async fn route(key: String) -> Result<u32, String> {
            Ok(counter_ref::get(vec![key]).await?.add(1))
        }
    }
}";

/// The view the macro derives for `import("example:counters/counter@1.0.0", keyed)`.
const VIEW: &str = "/// The objects of `example:counters/counter@1.0.0`, each selected by its key.
interface counter-ref {
  use example:counters/counter@1.0.0.{tally};
  /// An object of `example:counters/counter@1.0.0`: its methods are the interface's functions.
  resource counter {
    /// Add `amount` to the count.
    ///
    /// Return the new count.
    add: func(amount: u32) -> u32;
    read: async func() -> tally;
  }
  /// The object of `key` under the principal of the call, or why it was refused.
  get: async func(key: list<string>) -> result<counter, string>;
}
";

/// A directory holding one consumed WIT package, under this module's prefix.
fn consumed(name: &str, wit: &str) -> PathBuf {
    super::tests::consumed(&format!("keyed-{name}"), wit)
}

/// The options of `example:router@1.0.0` consuming `dir` and importing `keyed` by key.
fn options(dir: Option<&Path>, keyed: &[&str]) -> Options {
    Options {
        package: Some(literal("example:router@1.0.0")),
        wit: dir.map(|dir| dir.display().to_string()),
        keyed: keyed.iter().map(|identity| literal(identity)).collect(),
        ..Options::default()
    }
}

/// The JSON of the `tembra:manifest` section.
fn manifest(module: &ItemMod) -> String {
    super::tests::manifest(module).expect("the manifest")
}

/// The derived WIT read with the consumed packages, as the bindings read it.
fn resolved(dir: &Path, text: &str) {
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_dir(dir).unwrap();
    resolved_with(resolve, text);
}

/// An interface imported by key gets a view in the Orb's package: a resource of the interface's
/// name whose methods are its functions, `async` where they are and with their documentation, the
/// interface's types it names `use`d from it, and `get`. The world imports the view, the module
/// re-exports its bindings under the view's name and the manifest maps the view to the interface.
#[test]
fn a_keyed_import_derives_a_view_of_the_provider_interface() {
    let dir = consumed("view", COUNTERS);
    let module = expanded(
        options(Some(&dir), &["example:counters/counter@1.0.0"]),
        ROUTER,
    );
    let text = wit(&module);
    assert_eq!(
        text,
        format!(
            "package example:router@1.0.0;\n\ninterface routing {{\n  route: async func(key: string) -> result<u32, string>;\n}}\n\n{VIEW}\nworld router {{\n  import counter-ref;\n  export routing;\n}}\n"
        )
    );
    resolved(&dir, &text);
    assert_eq!(
        manifest(&module),
        format!(
            r#"{{"contributes":{{}},"declares":{{}},"services":{{}},"references":{{"example:router/counter-ref@1.0.0":"example:counters/counter@1.0.0"}},"wit":"{}"}}"#,
            dir.display()
        )
    );
    let tokens = quote::quote!(#module).to_string();
    assert!(
        tokens.contains("pub use self :: example :: router :: counter_ref ;"),
        "{tokens}"
    );
}

/// The plain import of the same interface stays as it was: it reaches the provider's `main`
/// object under its own name, and only the view enters the manifest's `references`; an Orb that
/// imports nothing by key writes its manifest without them.
#[test]
fn a_plain_import_stays_next_to_a_keyed_one() {
    let dir = consumed("plain", COUNTERS);
    let both = expanded(
        Options {
            imports: vec![literal("example:counters/counter@1.0.0")],
            ..options(Some(&dir), &["example:counters/counter@1.0.0"])
        },
        ROUTER,
    );
    let text = wit(&both);
    assert!(
        text.ends_with("world router {\n  import example:counters/counter@1.0.0;\n  import counter-ref;\n  export routing;\n}\n"),
        "{text}"
    );
    resolved(&dir, &text);
    let tokens = quote::quote!(#both).to_string();
    for reexport in [
        "pub use self :: example :: counters :: counter ;",
        "pub use self :: example :: router :: counter_ref ;",
    ] {
        assert!(tokens.contains(reexport), "{reexport}\n{tokens}");
    }
    assert!(
        manifest(&both).contains(
            r#""references":{"example:router/counter-ref@1.0.0":"example:counters/counter@1.0.0"}"#
        ),
        "{}",
        manifest(&both)
    );
    let plain = expanded(
        Options {
            imports: vec![literal("example:counters/counter@1.0.0")],
            ..options(Some(&dir), &[])
        },
        ROUTER,
    );
    assert_eq!(
        manifest(&plain),
        format!(
            r#"{{"contributes":{{}},"declares":{{}},"services":{{}},"wit":"{}"}}"#,
            dir.display()
        )
    );
}

/// Names the WIT spells with its `%` escape keep it in the view: a function, a parameter and a
/// type named after keywords of WIT.
#[test]
fn a_keyword_name_of_the_provider_keeps_its_escape_in_the_view() {
    let dir = consumed(
        "keywords",
        "package example:words@1.0.0;
interface book {
  record %record { at: u32 }
  %type: func(%from: list<%record>) -> option<tuple<%record, string>>;
}
",
    );
    let text = wit(&expanded(
        options(Some(&dir), &["example:words/book@1.0.0"]),
        ROUTER,
    ));
    assert!(
        text.contains("  use example:words/book@1.0.0.{%record};\n"),
        "{text}"
    );
    assert!(
        text.contains("    %type: func(%from: list<%record>) -> option<tuple<%record, string>>;\n"),
        "{text}"
    );
    resolved(&dir, &text);
}

/// `import("ns:name/interface@version", keyed)` is the attribute's keyed import; the identity
/// alone, another word than `keyed` or more after it is an error saying how to write it.
#[test]
fn the_attribute_writes_a_keyed_import_in_parentheses() {
    let dir = consumed("attribute", COUNTERS);
    let wit_dir = dir.display().to_string();
    let accepted = expansion(
        quote::quote!(package = "example:router@1.0.0", wit = #wit_dir, import("example:counters/counter@1.0.0", keyed)),
        syn::parse_str(ROUTER).unwrap(),
    )
    .to_string();
    // The macro accepts the module; its native side alone, inside the facade's wrapper, refuses
    // the keyed import, whose interface has a synchronous function: it works in a component build
    // only, so the native `orb()` answers that error.
    assert!(!accepted.contains("compile_error"), "{accepted}");
    assert!(
        accepted.contains(
            ":: tembra :: Error :: new (:: tembra :: core :: Code :: Unsupported , \"native\" , \"the import of example:counters/counter@1.0.0 by key works in a component build only: its function `add` is synchronous, and natively a method has no fiber to suspend while the kernel completes the call, so a native Orb imports only interfaces whose functions are `async func`\")"
        ),
        "{accepted}"
    );
    assert!(accepted.contains("import counter-ref;"), "{accepted}");
    for written in [
        quote::quote!(import("example:counters/counter@1.0.0")),
        quote::quote!(import("example:counters/counter@1.0.0", by_name)),
        quote::quote!(import("example:counters/counter@1.0.0", keyed, twice)),
    ] {
        let refused = expansion(
            quote::quote!(package = "example:router@1.0.0", wit = #wit_dir, #written),
            syn::parse_str(ROUTER).unwrap(),
        )
        .to_string();
        assert!(
            refused.contains("an interface imported by key is written import"),
            "{written}\n{refused}"
        );
    }
}

/// What makes a keyed import impossible is an error naming the interface: a WASI interface, which
/// the installation grants and no Orb provides, no directory to read it from, an interface that
/// is not there, one with a resource, a function carrying something other than data or data the
/// runtime refuses in a signature, no function at all, a function or a parameter named like what a
/// method of the view already has.
#[test]
fn an_interface_without_a_view_is_refused() {
    let wasi = refused(options(None, &["wasi:random/random@0.3.0"]), ROUTER);
    assert!(
        wasi.contains("wasi:random/random@0.3.0 is a WASI interface, which the installation grants and no Orb provides: import it with import = \"wasi:random/random@0.3.0\""),
        "{wasi}"
    );
    let missing = refused(options(None, &["example:counters/counter@1.0.0"]), ROUTER);
    assert!(
        missing.contains("importing an interface by key needs wit = \"<dir>\""),
        "{missing}"
    );
    let dir = consumed("refused", COUNTERS);
    let unknown = refused(
        options(Some(&dir), &["example:counters/missing@1.0.0"]),
        ROUTER,
    );
    assert!(
        unknown.contains("example:counters/missing@1.0.0 is not an interface of the WIT packages"),
        "{unknown}"
    );
    let invalid = refused(options(Some(&dir), &["example:counters"]), ROUTER);
    assert!(
        invalid.contains("an interface imported by key is named namespace:name/interface@version"),
        "{invalid}"
    );
    for (name, interface, message) in [
        (
            "resource",
            "resource cursor { next: func() -> u32; } open: func() -> cursor;",
            "`[method]cursor.next` is not a plain function",
        ),
        (
            "handle",
            "resource cursor; open: func() -> cursor;",
            "`open` carries a resource handle",
        ),
        (
            "held",
            "resource cursor; record held { cursor: cursor } open: func() -> held;",
            "`open` carries a resource handle in the type `held`",
        ),
        (
            "stream",
            "watch: async func() -> stream<u8>;",
            "`watch` carries a stream",
        ),
        // What tembra-contracts' `supported` refuses in a signature.
        (
            "nested",
            "maybe: func() -> option<option<u32>>;",
            "`maybe` carries a type the runtime refuses in a signature: nested options require a tagged option codec",
        ),
        (
            "map",
            "index: func(entries: map<string, u32>) -> u32;",
            "`index` carries a type the runtime refuses in a signature: map requires a dedicated interpreter value adapter",
        ),
        (
            "fixed",
            "corners: func() -> list<u32, 4>;",
            "`corners` carries a type the runtime refuses in a signature: fixed-length list requires a dedicated interpreter value adapter",
        ),
        (
            "nested-held",
            "record found { at: option<option<u32>> } find: func() -> found;",
            "`find` carries the type `found`, which the runtime refuses in a signature: nested options require a tagged option codec",
        ),
        ("empty", "record tally { count: u32 }", "has no function"),
        // A resource no function names: the kernel binds an interface with a resource as
        // callable, so the view's `use` of the interface would need a provider of its own.
        (
            "bare",
            "resource cursor; record tally { count: u32 } read: func() -> tally;",
            "has the resource `cursor`, and the objects of a view carry data, so an interface with a resource is not imported by key",
        ),
        // A method takes its object as `self`, which WIT does not tell from `SELF`.
        (
            "self",
            "add: func(self: u32) -> u32;",
            "`add` has a parameter `self`, and as a method of the objects a keyed import reaches it takes its object as `self`",
        ),
        (
            "upper-self",
            "add: func(amount: u32, SELF: u32) -> u32;",
            "`add` has a parameter `SELF`, and as a method of the objects a keyed import reaches it takes its object as `self`",
        ),
    ] {
        let dir = consumed(
            name,
            &format!("package example:shapes@1.0.0;\ninterface counter {{ {interface} }}\n"),
        );
        let refusal = refused(
            options(Some(&dir), &["example:shapes/counter@1.0.0"]),
            ROUTER,
        );
        assert!(
            refusal.contains("example:shapes/counter@1.0.0") && refusal.contains(message),
            "{name}: {refusal}"
        );
    }
    // A resource `use`d from another interface is the interface's as well.
    let dir = consumed(
        "used",
        "package example:shapes@1.0.0;
interface cursors { resource cursor; }
interface counter { use cursors.{cursor}; read: func() -> u32; }
",
    );
    let used = refused(
        options(Some(&dir), &["example:shapes/counter@1.0.0"]),
        ROUTER,
    );
    assert!(
        used.contains(
            "example:shapes/counter@1.0.0 cannot be imported by key: it has the resource `cursor`"
        ),
        "{used}"
    );
}

/// An interface the module implements is imported by key, not plainly: a proxy serving the
/// interface reaches another provider's objects through the view, while a world that imports the
/// interface itself and exports it is a hook, so a plain import is an error on the import. The view
/// `use`s the interface's types, so the elaborated world imports the interface it exports, for the
/// types alone, which the contracts and `cargo tembra build` tell from a hook, an interface the
/// world imports itself. A derived signature names the interface's types through the implemented
/// interface's module, the export's copy, which the derived interface takes; the view's module
/// names the import's.
#[test]
fn an_implemented_interface_is_imported_by_key() {
    const PROXY: &str = "mod proxy {
        #[service(\"example:counters/counter@1.0.0\")]
        pub struct Counter;
        impl Counter {
            pub fn add(amount: u32) -> u32 { amount }
            pub async fn read() -> counter::Tally { counter::Tally { key: vec![], count: 0 } }
        }
    }";
    let dir = consumed("implemented", COUNTERS);
    // Imported plainly, the interface would be both the import and the export the Orb module
    // re-exports as `counter`, in a world that pairs them as a hook.
    let plain = refused(
        Options {
            imports: vec![literal("example:counters/counter@1.0.0")],
            ..options(Some(&dir), &[])
        },
        PROXY,
    );
    assert!(
        plain.contains("example:counters/counter@1.0.0 is implemented by this module and imported plainly: a world that imports and exports one interface is a hook; reach another provider's objects with import(\"example:counters/counter@1.0.0\", keyed), or contribute to or declare the hook with #[hook(\"example:counters/counter@1.0.0\", ..)]"),
        "{plain}"
    );
    // The proxy implements the interface and reaches another provider's objects by key.
    expanded(
        options(Some(&dir), &["example:counters/counter@1.0.0"]),
        PROXY,
    );
    // Through the view, from a derived service.
    let relaying = PROXY.replace(
        "\n    }",
        "
        #[service]
        pub struct Relay;
        impl Relay {
            /// The tally of the object of `key` of the provider the view reaches.
            pub async fn relayed(key: String) -> Result<counter::Tally, String> {
                let tally = counter_ref::get(vec![key]).await?.read().await;
                Ok(counter::Tally { key: tally.key, count: tally.count })
            }
        }
    }",
    );
    let module = expanded(
        options(Some(&dir), &["example:counters/counter@1.0.0"]),
        &relaying,
    );
    let text = wit(&module);
    for expected in [
        "interface relay {\n  use example:counters/counter@1.0.0.{tally};\n  /// The tally of the object of `key` of the provider the view reaches.\n  relayed: async func(key: string) -> result<tally, string>;\n}\n".to_owned(),
        format!("{VIEW}\nworld proxy {{\n  import counter-ref;\n  export example:counters/counter@1.0.0;\n  export relay;\n}}\n"),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    resolved(&dir, &text);
    assert!(
        manifest(&module).contains(
            r#""references":{"example:router/counter-ref@1.0.0":"example:counters/counter@1.0.0"}"#
        ),
        "{}",
        manifest(&module)
    );
    let tokens = quote::quote!(#module).to_string();
    for reexport in [
        "pub use exports :: example :: counters :: counter ;",
        "pub use self :: example :: router :: counter_ref ;",
    ] {
        assert!(tokens.contains(reexport), "{reexport}\n{tokens}");
    }
}

/// The keyed import of an implemented interface brings the bindings' import module of the
/// interface, `self::example::counters::counter`, which names the import's copy of its types, as
/// the view's module does: a derived signature names an implemented interface's types through the
/// interface's own module, `counter::Tally`, the export's copy, or through the export module the
/// Orb module re-exports it from; rustc reports a path into the bindings' import on the signature.
#[test]
fn a_derived_signature_names_an_implemented_interfaces_types_through_its_export() {
    let dir = consumed("bindings-import", COUNTERS);
    let proxy = |relayed: &str| {
        format!(
            "mod proxy {{
                #[service(\"example:counters/counter@1.0.0\")]
                pub struct Counter;
                impl Counter {{
                    pub fn add(amount: u32) -> u32 {{ amount }}
                    pub async fn read() -> counter::Tally {{ counter::Tally {{ key: vec![], count: 0 }} }}
                }}
                #[service]
                pub struct Relay;
                impl Relay {{ {relayed} }}
            }}"
        )
    };
    for path in [
        "counter::Tally",
        "exports::example::counters::counter::Tally",
    ] {
        expanded(
            options(Some(&dir), &["example:counters/counter@1.0.0"]),
            &proxy(&format!(
                "pub fn relayed(last: {path}) -> u32 {{ last.count }}"
            )),
        );
    }
}

/// An implemented interface may `use` its types from an interface the world only imports: the
/// bindings alias the implemented interface's import and export, and the view of its keyed
/// import, to the type of that import, so the view and the bindings' module of the import name
/// the very type a derived interface takes.
#[test]
fn a_type_an_implemented_interface_uses_from_an_import_has_one_copy() {
    let dir = consumed(
        "used-tally",
        "package example:counters@1.0.0;

interface types {
  record tally { key: list<string>, count: u32 }
}

interface counter {
  use types.{tally};
  add: func(amount: u32) -> u32;
  read: async func() -> tally;
}
",
    );
    let module = expanded(
        options(Some(&dir), &["example:counters/counter@1.0.0"]),
        "mod proxy {
            #[service(\"example:counters/counter@1.0.0\")]
            pub struct Counter;
            impl Counter {
                pub fn add(amount: u32) -> u32 { amount }
                pub async fn read() -> counter::Tally { counter::Tally { key: vec![], count: 0 } }
            }
            #[service]
            pub struct Relay;
            impl Relay {
                pub async fn relayed(key: String) -> Result<counter_ref::Tally, String> {
                    Ok(counter_ref::get(vec![key]).await?.read().await)
                }
                pub fn counted(last: self::example::counters::counter::Tally) -> u32 { last.count }
            }
        }",
    );
    let text = wit(&module);
    assert!(
        text.contains("interface relay {\n  use example:counters/counter@1.0.0.{tally};\n"),
        "{text}"
    );
    resolved(&dir, &text);
}

/// A view is an interface of the Orb's package named after the provider interface: an interface
/// imported by key twice, two interfaces of one name, a view named like the world or a service's
/// interface, and a provider type named like the view's resource or `get`, are errors.
#[test]
fn a_view_takes_a_name_of_its_own() {
    let dir = consumed(
        "names",
        &format!(
            "{COUNTERS}
package example:other@1.0.0 {{
  interface counter {{ add: func(amount: u32) -> u32; }}
  interface clash {{ record clash {{ at: u32 }} read: func() -> clash; }}
  interface getter {{ record get {{ at: u32 }} read: func() -> get; }}
}}
"
        ),
    );
    for (keyed, module, message) in [
        (
            &[
                "example:counters/counter@1.0.0",
                "example:counters/counter@1.0.0",
            ][..],
            ROUTER.to_owned(),
            "example:counters/counter@1.0.0 is imported by key twice",
        ),
        (
            &[
                "example:counters/counter@1.0.0",
                "example:other/counter@1.0.0",
            ][..],
            ROUTER.to_owned(),
            "example:other/counter@1.0.0 and example:counters/counter@1.0.0 both have their view in the interface `counter-ref`",
        ),
        (
            &["example:counters/counter@1.0.0"][..],
            ROUTER.replace("mod router", "mod counter_ref"),
            "the view of example:counters/counter@1.0.0, the interface `counter-ref`, has the name of the world",
        ),
        (
            &["example:counters/counter@1.0.0"][..],
            ROUTER.replace("Routing", "CounterRef"),
            "has the name of the interface of `CounterRef`",
        ),
        (
            &["example:other/clash@1.0.0"][..],
            ROUTER.to_owned(),
            "example:other/clash@1.0.0 names a type `clash`, which its view, the interface `clash-ref`, gives its resource",
        ),
        (
            &["example:other/getter@1.0.0"][..],
            ROUTER.to_owned(),
            "names a type `get`, which its view, the interface `getter-ref`, gives its acquisition function",
        ),
    ] {
        let refusal = refused(options(Some(&dir), keyed), &module);
        assert!(refusal.contains(message), "{message}\n{refusal}");
    }
}

/// The Orb module re-exports a view's module under the view's name, and an imported interface's
/// under the interface's: an import of an interface named like a view is an error naming both,
/// and either import alone expands.
#[test]
fn an_import_named_like_a_view_is_refused() {
    let dir = consumed(
        "reexported",
        &format!(
            "{COUNTERS}
package example:other@1.0.0 {{
  interface counter-ref {{ read: func() -> u32; }}
}}
"
        ),
    );
    let plain = || vec![literal("example:other/counter-ref@1.0.0")];
    let refusal = refused(
        Options {
            imports: plain(),
            ..options(Some(&dir), &["example:counters/counter@1.0.0"])
        },
        ROUTER,
    );
    assert!(
        refusal.contains("the Orb module would re-export two modules as `counter_ref`: that of the imported interface example:other/counter-ref@1.0.0, and that of the interface example:counters/counter@1.0.0, which this module imports by key; keep one of them"),
        "{refusal}"
    );
    expanded(
        options(Some(&dir), &["example:counters/counter@1.0.0"]),
        ROUTER,
    );
    expanded(
        Options {
            imports: plain(),
            ..options(Some(&dir), &[])
        },
        ROUTER,
    );
}
