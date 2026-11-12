//! Keyed imports: the view the macro derives from the provider's WIT, the role it embeds, the
//! module it re-exports and the imports it refuses.
use super::{Options, expand, expansion};
use std::path::{Path, PathBuf};
use syn::{Expr, Item, ItemMod, Lit, LitStr, UnOp};

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

fn literal(text: &str) -> LitStr {
    LitStr::new(text, proc_macro2::Span::call_site())
}

/// A directory holding one consumed WIT package, under the system's temporary directory.
fn consumed(name: &str, wit: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tembra-orb-macro-keyed-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.wit"), wit).unwrap();
    dir
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

fn expanded(options: Options, module: &str) -> ItemMod {
    let module = syn::parse_str(module).expect("a module");
    syn::parse2(expand(options, module).unwrap_or_else(|error| panic!("{error}"))).unwrap()
}

fn refused(options: Options, module: &str) -> String {
    match expand(options, syn::parse_str(module).expect("a module")) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("the module was accepted"),
    }
}

fn items(module: &ItemMod) -> &[Item] {
    &module.content.as_ref().expect("a body").1
}

/// The derived WIT, from the generated `WIT` constant.
fn wit(module: &ItemMod) -> String {
    items(module)
        .iter()
        .find_map(|item| match item {
            Item::Const(constant) if constant.ident == "WIT" => match &*constant.expr {
                Expr::Lit(literal) => match &literal.lit {
                    Lit::Str(text) => Some(text.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .expect("the WIT constant")
}

/// The JSON of the `tembra:manifest` section.
fn manifest(module: &ItemMod) -> String {
    items(module)
        .iter()
        .find_map(|item| match item {
            Item::Static(section) if section.ident == "__TEMBRA_MANIFEST" => match &*section.expr {
                Expr::Unary(deref) if matches!(deref.op, UnOp::Deref(_)) => match &*deref.expr {
                    Expr::Lit(literal) => match &literal.lit {
                        Lit::ByteStr(bytes) => Some(String::from_utf8(bytes.value()).unwrap()),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .expect("the manifest")
}

/// The derived WIT read with the consumed packages, as the bindings read it.
fn resolved(dir: &Path, text: &str) {
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_dir(dir).unwrap();
    resolve
        .push_str("orb.wit", text)
        .unwrap_or_else(|error| panic!("{error:#}\n{text}"));
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
    assert!(!accepted.contains("compile_error"), "{accepted}");
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
/// is not there, one with a resource, a function carrying something other than data, no function
/// at all.
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
        ("empty", "record tally { count: u32 }", "has no function"),
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
