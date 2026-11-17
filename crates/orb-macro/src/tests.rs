//! The macro's reading of a module, checked on its output: the WIT it derives, the manifest it
//! embeds and the errors it reports. The generated bindings themselves compile in the examples.
//! The helpers that expand a module and read its output serve the other test modules too.
use super::{Options, expand, expansion, metadata::Metadata, primary_crate_dir};
use syn::{Expr, Item, ItemMod, Lit, UnOp};

/// Expand a module under `#[orb(package = ...)]` and parse the generated module back.
fn orb(package: &str, module: &str) -> syn::Result<ItemMod> {
    orb_with(options(package, None), module)
}

/// A string literal, as written on the attribute.
pub(crate) fn literal(text: &str) -> syn::LitStr {
    syn::LitStr::new(text, proc_macro2::Span::call_site())
}

fn options(package: &str, wit: Option<&std::path::Path>) -> Options {
    Options {
        package: Some(literal(package)),
        wit: wit.map(|dir| dir.display().to_string()),
        context: false,
        no_std: false,
        facade: None,
        imports: Vec::new(),
        keyed: Vec::new(),
    }
}

/// A directory holding one consumed WIT package, under the system's temporary directory. Another
/// test module starts `name` with a prefix of its own: the modules use the same names, and the
/// tests run at once.
pub(crate) fn consumed(name: &str, wit: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tembra-orb-macro-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.wit"), wit).unwrap();
    dir
}

/// The generated module as one token string, for looking up generated items.
pub(crate) fn tokens(module: &ItemMod) -> String {
    quote::quote!(#module).to_string()
}

fn orb_with(options: Options, module: &str) -> syn::Result<ItemMod> {
    orb_as(options, Metadata::named("book"), module)
}

/// Expand a module with the given options and manifest table, and parse the generated module back.
fn orb_as(options: Options, metadata: Metadata, module: &str) -> syn::Result<ItemMod> {
    let module: ItemMod = syn::parse_str(module).expect("a module");
    syn::parse2(expand(options, module, metadata, None)?)
}

fn expanded(package: &str, module: &str) -> ItemMod {
    orb(package, module).unwrap_or_else(|error| panic!("{error}"))
}

pub(crate) fn expanded_with(options: Options, module: &str) -> ItemMod {
    orb_with(options, module).unwrap_or_else(|error| panic!("{error}"))
}

fn refused(package: &str, module: &str) -> String {
    refused_with(options(package, None), module)
}

pub(crate) fn refused_with(options: Options, module: &str) -> String {
    match orb_with(options, module) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("the module was accepted"),
    }
}

fn items(module: &ItemMod) -> &[Item] {
    &module.content.as_ref().expect("a body").1
}

/// The derived WIT, from the generated `WIT` constant.
pub(crate) fn wit(module: &ItemMod) -> String {
    constant(module, "WIT")
}

/// The text of a generated string constant of the module.
fn constant(module: &ItemMod, name: &str) -> String {
    items(module)
        .iter()
        .find_map(|item| match item {
            Item::Const(constant) if constant.ident == name => match &*constant.expr {
                Expr::Lit(literal) => match &literal.lit {
                    Lit::Str(text) => Some(text.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("the {name} constant"))
}

/// The JSON of the `tembra:manifest` section, if the module embeds one.
pub(crate) fn manifest(module: &ItemMod) -> Option<String> {
    items(module).iter().find_map(|item| match item {
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
}

/// The items the module's native side wraps in `::tembra::__host!`, in the module itself, for an
/// empty `path`, or in the sub-module at `path`, as one token string, once they parse as items;
/// empty where the module has none.
fn native(module: &ItemMod, path: &[&str]) -> String {
    let mut items = items(module);
    for segment in path {
        items = items
            .iter()
            .find_map(|item| match item {
                Item::Mod(inner) if inner.ident == segment => {
                    Some(&inner.content.as_ref().expect("a body").1[..])
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the sub-module `{segment}`"));
    }
    items
        .iter()
        .filter_map(|item| match item {
            Item::Macro(invocation) => {
                let path = &invocation.mac.path;
                (quote::quote!(#path).to_string() == ":: tembra :: __host").then(|| {
                    let tokens = &invocation.mac.tokens;
                    syn::parse2::<syn::File>(tokens.clone()).unwrap_or_else(|error| {
                        panic!("the native items do not parse: {error}\n{tokens}")
                    });
                    tokens.to_string()
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const PACKAGE: &str = "example:book@1.0.0";

fn ledger(attribute: &str) -> String {
    format!(
        "mod book {{
            {attribute}
            pub struct Ledger;
            impl Ledger {{
                /// How many entries the ledger holds.
                pub fn count() -> u32 {{ 0 }}
            }}
        }}"
    )
}

#[test]
fn a_plain_service_derives_its_interface_and_embeds_no_manifest() {
    let module = expanded(PACKAGE, &ledger("#[service]"));
    assert_eq!(
        wit(&module),
        "package example:book@1.0.0;\n\ninterface ledger {\n  /// How many entries the ledger holds.\n  count: func() -> u32;\n}\n\nworld book {\n  export ledger;\n}\n"
    );
    assert_eq!(manifest(&module), None);
}

#[test]
fn the_bindings_come_from_the_wit_bindgen_the_facade_pins() {
    // The generated code follows the derive rules and the flags lowering of one wit-bindgen
    // version, so the facade picks it, not the author's crate: the expansion calls `generate!`
    // through the facade's re-export, reaches the bindings' runtime there, and names no
    // `wit_bindgen` crate of the author's.
    let generated = tokens(&expanded(PACKAGE, &ledger("#[service]")));
    for expected in [
        ":: tembra :: __wit_bindgen :: generate !",
        "runtime_path : \"::tembra::__wit_bindgen::rt\"",
        "bitflags_path : \"::tembra::__wit_bindgen::rt::bitflags\"",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    assert!(!generated.contains(":: wit_bindgen"), "{generated}");
}

/// A guest that depends on the facade under another name, `sdk = { package = "tembra", .. }`,
/// names it with `crate = ".."` on the attribute, as serde and tokio take theirs: every path the
/// expansion writes through the facade takes that name, while the bindings' modules of the kernel's
/// `tembra:guest` package keep theirs. Without the option the name is `tembra`. A value that is not
/// a name the crate's code can write is refused at the literal.
#[test]
fn the_crate_option_names_the_facade_dependency() {
    use proc_macro2::TokenStream;
    use quote::quote;
    let attribute = |tail: TokenStream| quote!(package = "example:counter@1.0.0", context #tail);
    let generated = |tail: TokenStream| {
        expansion(attribute(tail), syn::parse_str(COUNTER).unwrap()).to_string()
    };
    let plain = generated(quote!());
    assert_eq!(generated(quote!(, crate = "tembra")), plain);
    let renamed = generated(quote!(, crate = "sdk"));
    for expected in [
        ":: sdk :: __wit_bindgen :: generate !",
        "runtime_path : \"::sdk::__wit_bindgen::rt\"",
        "bitflags_path : \"::sdk::__wit_bindgen::rt::bitflags\"",
        "exports :: tembra :: guest :: state_init",
        "pub use self :: tembra :: guest :: context ;",
    ] {
        assert!(renamed.contains(expected), "{expected}\n{renamed}");
    }
    assert_eq!(
        renamed,
        plain
            .replace(":: tembra :: __wit_bindgen", ":: sdk :: __wit_bindgen")
            .replace("\"::tembra::__wit_bindgen", "\"::sdk::__wit_bindgen")
    );
    for value in ["my-sdk", "::sdk", "sdk::orb", "crate", ""] {
        let error = match super::options(attribute(quote!(, crate = #value))) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("`{value}` was accepted"),
        };
        assert_eq!(
            error,
            "`crate` names the facade dependency as the crate's code writes it, a Rust identifier: #[sdk::orb(crate = \"sdk\", ..)] for `sdk = { package = \"tembra\", .. }`",
            "{value}"
        );
    }
    let error = match super::options(attribute(quote!(, krate = "sdk"))) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("`krate` was accepted"),
    };
    assert!(
        error.contains("context, no_std, crate = \"name\")]"),
        "{error}"
    );
}

#[test]
fn a_service_lifetime_goes_to_the_manifest_and_leaves_the_wit_alone() {
    let plain = wit(&expanded(PACKAGE, &ledger("#[service]")));
    let module = expanded(PACKAGE, &ledger("#[service(lifetime = host)]"));
    assert_eq!(wit(&module), plain);
    assert_eq!(
        manifest(&module).as_deref(),
        Some(
            r#"{"contributes":{},"declares":{},"services":{"example:book/ledger@1.0.0":{"lifetime":"host","linger_ms":0}}}"#
        )
    );
    let module = expanded(PACKAGE, &ledger("#[service(linger_ms = 2500)]"));
    assert_eq!(
        manifest(&module).as_deref(),
        Some(
            r#"{"contributes":{},"declares":{},"services":{"example:book/ledger@1.0.0":{"lifetime":"referenced","linger_ms":2500}}}"#
        )
    );
}

/// wit-bindgen's `export!` defines the component's exports as symbols, the kernel's `state-init`
/// and `resources-init` callbacks included, which two Orb crates linked into one native binary
/// would both define: only a component build invokes it.
#[test]
fn only_a_component_build_exports() {
    let module = expanded(
        "example:vault@1.0.0",
        "mod safe {
            #[state]
            pub struct Sealed { pub count: u32 }
            #[resource]
            pub struct Banner { pub text: String }
            impl Banner { #[open] pub fn open() -> Self { Banner { text: String::new() } } }
            #[service]
            pub struct Vault;
            impl Vault { pub fn get() -> u32 { 0 } }
        }",
    );
    let exports: Vec<&syn::ItemMacro> = items(&module)
        .iter()
        .filter_map(|item| match item {
            Item::Macro(invocation) if invocation.mac.path.is_ident("export") => Some(invocation),
            _ => None,
        })
        .collect();
    let generated = tokens(&module);
    assert_eq!(exports.len(), 1, "{generated}");
    assert_eq!(generated.matches("export !").count(), 1, "{generated}");
    let gate = &exports[0].attrs;
    assert_eq!(
        quote::quote!(#(#gate)*).to_string(),
        "# [cfg (target_arch = \"wasm32\")]"
    );
}

/// `context` on the attribute imports the kernel's context interface, alone, and re-exports its
/// generated module under its own name in a component build, through the facade's `__guest!`: a
/// native host gets the native side's `context` instead.
#[test]
fn the_context_option_imports_the_kernel_interface() {
    let module = orb_with(
        Options {
            context: true,
            ..options(PACKAGE, None)
        },
        &ledger("#[service]"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        wit(&module)
            .contains("world book {\n  import tembra:guest/context@1.0.0;\n  export ledger;\n}"),
        "{}",
        wit(&module)
    );
    let guest: Vec<String> = items(&module)
        .iter()
        .filter_map(|item| match item {
            Item::Macro(invocation) => {
                let path = &invocation.mac.path;
                (quote::quote!(#path).to_string() == ":: tembra :: __guest")
                    .then(|| invocation.mac.tokens.to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(guest, ["pub use self :: tembra :: guest :: context ;"]);
    let plain = expanded(PACKAGE, &ledger("#[service]"));
    assert!(!wit(&plain).contains("context"));
    assert!(!tokens(&plain).contains("__guest"));
}

/// The Orb module re-exports the kernel's context interface as `context` for the `context` option:
/// an imported or an implemented interface named `context` beside it is an error naming both, an
/// import of the kernel's interface itself is an error asking to remove it, and each expands
/// without the option.
#[test]
fn an_interface_named_context_is_refused_beside_the_context_option() {
    let dir = consumed(
        "notes-context",
        "package example:notes@1.0.0;\ninterface context {\n  topic: func() -> string;\n}\n",
    );
    // The kernel's package, as a contract's `deps/` holds it, for the import of its interface.
    std::fs::create_dir_all(dir.join("deps")).unwrap();
    std::fs::write(
        dir.join("deps").join("tembra-guest.wit"),
        tembra_contracts::guest::SOURCE,
    )
    .unwrap();
    let kernel = "and that of the kernel's interface tembra:guest/context@1.0.0, which the `context` option imports; keep one of them";
    let importing = |identity: &str, context: bool| Options {
        imports: vec![literal(identity)],
        context,
        ..options(PACKAGE, Some(&dir))
    };
    for (identity, message) in [
        (
            "example:notes/context@1.0.0",
            format!("the Orb module would re-export two modules as `context`: that of the imported interface example:notes/context@1.0.0, {kernel}"),
        ),
        (
            "tembra:guest/context@1.0.0",
            "the `context` option imports the kernel's interface tembra:guest/context@1.0.0 already, which the Orb module re-exports as `context`; remove this import".to_owned(),
        ),
    ] {
        let error = refused_with(importing(identity, true), &ledger("#[service]"));
        assert!(error.contains(&message), "{identity}: {error}");
        expanded_with(importing(identity, false), &ledger("#[service]"));
    }
    let topics = "mod topics {
        #[service(\"example:notes/context@1.0.0\")]
        pub struct Topics;
        impl Topics { pub fn topic() -> String { String::new() } }
    }";
    let implementing = |context| Options {
        context,
        ..options("example:topics@1.0.0", Some(&dir))
    };
    let error = refused_with(implementing(true), topics);
    assert!(
        error.contains(&format!("the Orb module would re-export two modules as `context`: that of the implemented interface example:notes/context@1.0.0, {kernel}")),
        "{error}"
    );
    expanded_with(implementing(false), topics);
}

const COUNTER: &str = "mod tally {
    #[state]
    pub struct Count { pub total: i64 }
    impl Count {
        #[validate]
        pub fn validate(&self) -> Result<(), String> {
            if self.total < 0 { Err(String::from(\"negative\")) } else { Ok(()) }
        }
    }
    #[service]
    pub struct Counter;
    impl Counter {
        pub fn add(by: i64) -> Result<Count, String> { Count::update(|count| count.total += by) }
    }
}";

/// A state with a function marked `#[validate]` runs it on a reload and its `update` answers the
/// refusal; the marker decides, not the name, and the function has one shape. Every state declares
/// itself validated, so a reload checks that the inherited value reads as the state's type.
#[test]
fn a_validated_state_declares_so_and_updates_fallibly() {
    let module = expanded("example:counter@1.0.0", COUNTER);
    let generated = tokens(&module);
    assert!(generated.contains("validated : true"), "{generated}");
    assert!(generated.contains("value . validate () ? ;"), "{generated}");
    assert!(
        generated.contains("Ok (state) => state . validate () ,"),
        "{generated}"
    );
    assert!(!generated.contains("# [validate]"), "{generated}");
    assert!(
        wit(&module).contains("add: func(by: s64) -> result<count, string>;"),
        "{}",
        wit(&module)
    );
    // The marked function may have any name and stay private.
    let renamed = tokens(&expanded(
        "example:counter@1.0.0",
        &COUNTER.replace(
            "pub fn validate(&self) -> Result<(), String>",
            "fn non_negative(&self) -> Result<(), String>",
        ),
    ));
    assert!(renamed.contains("validated : true"), "{renamed}");
    assert!(renamed.contains("value . non_negative () ? ;"), "{renamed}");
    assert!(
        renamed.contains("Ok (state) => state . non_negative () ,"),
        "{renamed}"
    );
    // Without the marker, `validate` is an ordinary function: a reload still reads the inherited
    // value as the state's type, and runs no rule.
    let plain = tokens(&expanded(
        "example:counter@1.0.0",
        &COUNTER.replace("#[validate]\n", ""),
    ));
    assert!(plain.contains("validated : true"), "{plain}");
    assert!(!plain.contains("validated : false"), "{plain}");
    assert!(
        plain.contains("\"example:counter/counter@1.0.0\" => match :: serde_json :: from_str :: < Count > (& value) { :: core :: result :: Result :: Ok (_) => :: core :: result :: Result :: Ok (()) , :: core :: result :: Result :: Err (error) =>"),
        "{plain}"
    );
    assert!(plain.contains("pub fn validate (& self)"), "{plain}");
    assert!(!plain.contains("value . validate ()"), "{plain}");
    assert!(!plain.contains("state . validate ()"), "{plain}");
    for wrong in [
        "pub fn validate(&mut self) -> Result<(), String>",
        "pub fn validate(&self) -> bool",
        "pub fn validate() -> Result<(), String>",
    ] {
        let error = refused(
            "example:counter@1.0.0",
            &COUNTER.replace("pub fn validate(&self) -> Result<(), String>", wrong),
        );
        assert!(error.contains("#[validate] marks"), "{wrong}: {error}");
    }
    let error = refused(
        "example:counter@1.0.0",
        &COUNTER.replace(
            "#[validate]\n        pub fn validate",
            "#[validate]\n        pub fn other(&self) -> Result<(), String> { Ok(()) }\n        #[validate]\n        pub fn validate",
        ),
    );
    assert!(
        error.contains("two functions marked #[validate]"),
        "{error}"
    );
}

/// A value whose JSON would not read back as the state, a NaN float for one, never reaches the
/// kernel: `update` checks the round trip first and a refusal leaves the stored value in place,
/// answered like the rule's own when the state has one and trapping otherwise; the declaration
/// checks the initial value the same way.
#[test]
fn a_state_json_cannot_hold_is_never_stored() {
    let validated = tokens(&expanded("example:counter@1.0.0", COUNTER));
    for expected in [
        "value . validate () ? ; __tembra_state :: update (& value , \"Count\") }",
        "initial : __tembra_state :: initial (& < Count as :: core :: default :: Default > :: default () , \"Count\") ,",
        "let json = storable (value , name) ? ; Ok (parse (& super :: tembra :: guest :: state :: update (& json)))",
        "match :: serde_json :: from_str :: < T > (& json) { Ok (_) => Ok (json) , Err (error) => Err (format ! (",
        "Err (refusal) => panic ! (\"{}\" , refusal) ,",
    ] {
        assert!(validated.contains(expected), "{expected}\n{validated}");
    }
    assert!(!validated.contains("to_json"), "{validated}");
    let plain = tokens(&expanded(
        "example:counter@1.0.0",
        &COUNTER.replace("#[validate]\n", ""),
    ));
    assert!(
        plain.contains("change (& mut value) ; match __tembra_state :: update (& value , \"Count\") { :: core :: result :: Result :: Ok (stored) => stored , :: core :: result :: Result :: Err (refusal) => :: core :: panic ! (\"{}\" , refusal) , }"),
        "{plain}"
    );
}

/// A marker goes on a function of a struct of its kind, in an inherent impl block, and a
/// function carries one role.
#[test]
fn a_marker_goes_where_it_belongs() {
    let error = refused(
        "example:counter@1.0.0",
        &COUNTER.replace("pub fn add(by: i64)", "#[validate] pub fn add(by: i64)"),
    );
    assert!(
        error.contains("#[validate] marks a function of a #[state] struct; `Counter` is not one"),
        "{error}"
    );
    let error = refused(
        "example:counter@1.0.0",
        &COUNTER.replace("#[service]", "#[open] pub fn free() {}\n    #[service]"),
    );
    assert!(error.contains("not on a free function"), "{error}");
    let error = refused(
        "example:counter@1.0.0",
        &COUNTER.replace(
            "#[service]",
            "impl Default for Count { #[validate] fn default() -> Self { Count { total: 0 } } }\n    #[service]",
        ),
    );
    assert!(error.contains("not in a trait impl"), "{error}");
    let error = refused(
        "example:counter@1.0.0",
        &COUNTER.replace("#[validate]", "#[validate] #[open]"),
    );
    assert!(error.contains("carries two markers"), "{error}");
}

const FILTER: &str = "mod filter {
    #[resource]
    pub struct Counts { pub seen: u32 }
    impl Counts {
        #[open]
        pub fn open() -> Counts { Counts { seen: 0 } }
        #[close]
        fn close(self) {}
    }
    #[service]
    pub struct Report;
    impl Report {
        pub fn seen() -> u32 { Counts::with(|counts| counts.seen) }
    }
}";

/// A root resource is opened and closed by its marked functions, whatever their names; `#[open]`
/// is required, `#[close]` optional, and each has one shape.
#[test]
fn a_root_resource_is_opened_and_closed_by_its_marked_functions() {
    let module = expanded("example:filter@1.0.0", FILTER);
    assert!(
        wit(&module).contains(
            "world filter {\n  import tembra:guest/resources@1.0.0;\n  export tembra:guest/resources-init@1.0.0;\n  export report;\n}"
        ),
        "{}",
        wit(&module)
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("__TEMBRA_RESOURCE_COUNTS . open (Counts :: open ()) ;"),
        "{generated}"
    );
    assert!(generated.contains("{ value . close () ; }"), "{generated}");
    assert!(!generated.contains("# [open]"), "{generated}");
    let renamed = tokens(&expanded(
        "example:filter@1.0.0",
        &FILTER
            .replace("pub fn open() -> Counts", "fn start() -> Self")
            .replace("fn close(self) {}", "pub fn stop(self) {}"),
    ));
    assert!(
        renamed.contains("__TEMBRA_RESOURCE_COUNTS . open (Counts :: start ()) ;"),
        "{renamed}"
    );
    assert!(renamed.contains("{ value . stop () ; }"), "{renamed}");
    let unclosed = tokens(&expanded(
        "example:filter@1.0.0",
        &FILTER.replace("#[close]\n", ""),
    ));
    assert!(
        unclosed.contains("{ __TEMBRA_RESOURCE_COUNTS . close () ; }"),
        "{unclosed}"
    );
    let error = refused("example:filter@1.0.0", &FILTER.replace("#[open]\n", ""));
    assert!(error.contains("needs a function marked #[open]"), "{error}");
    let error = refused(
        "example:filter@1.0.0",
        &FILTER.replace(
            "pub fn open() -> Counts",
            "pub fn open(seen: u32) -> Counts",
        ),
    );
    assert!(error.contains("#[open] marks"), "{error}");
    let error = refused(
        "example:filter@1.0.0",
        &FILTER.replace("fn close(self) {}", "fn close(&self) {}"),
    );
    assert!(error.contains("#[close] marks"), "{error}");
}

#[test]
fn a_host_lifetime_cannot_linger_and_unknown_options_are_refused() {
    let error = refused(
        PACKAGE,
        &ledger("#[service(lifetime = host, linger_ms = 5)]"),
    );
    assert!(error.contains("cannot linger"), "{error}");
    let error = refused(PACKAGE, &ledger("#[service(lifetime = forever)]"));
    assert!(
        error.contains("referenced (the default) or host"),
        "{error}"
    );
    let error = refused(PACKAGE, &ledger("#[service(linger = 5)]"));
    assert!(error.contains("unknown option"), "{error}");
}

/// The kernel's bundled package nests the same from a checkout whose line breaks are `\r\n`: the
/// declaration opens the block, the rest of the file goes inside, and a world resolves it.
#[test]
fn the_kernel_package_nests_from_either_line_break() {
    const GUEST: &str = tembra_contracts::guest::SOURCE;
    let lf = GUEST.replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    let nested = super::wit::nested(&lf).expect("a declaration");
    assert_eq!(super::wit::nested(&crlf).as_deref(), Some(nested.as_str()));
    let (declaration, body) = lf.split_once(";\n").expect("a declaration");
    assert_eq!(
        nested,
        format!("{declaration} {{\n{}}}\n", body.trim_start_matches('\n'))
    );
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str(
            "orb.wit",
            &format!(
                "package example:orb@1.0.0;\nworld orb {{\n  import tembra:guest/state@1.0.0;\n}}\n{nested}"
            ),
        )
        .unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(super::wit::nested("interface lonely {}\n"), None);
}

/// A side of a `Result` that carries nothing is left out, as WIT spells it: `result<t>` for
/// `Result<T, ()>`, `result<_, e>` for `Result<(), E>` and `result` for `Result<(), ()>`, the
/// spellings the bindings read back as the author's types. `Option<()>` and `()` as a value have
/// no WIT equivalent.
#[test]
fn a_side_of_a_result_that_carries_nothing_is_left_out() {
    let method = |result: &str| {
        ledger("#[service]").replace(
            "pub fn count() -> u32 { 0 }",
            &format!("pub fn parse(text: String) -> {result} {{ todo!() }}"),
        )
    };
    for (rust, spelled) in [
        ("Result<u32, ()>", "result<u32>"),
        ("Result<(), String>", "result<_, string>"),
        ("Result<(), ()>", "result"),
        ("Result<u32, String>", "result<u32, string>"),
        ("Option<Result<(), ()>>", "option<result>"),
    ] {
        let text = wit(&expanded(PACKAGE, &method(rust)));
        assert!(
            text.contains(&format!("  parse: func(text: string) -> {spelled};\n")),
            "{rust}: {text}"
        );
        let mut resolve = wit_parser::Resolve::default();
        resolve
            .push_str("book.wit", &text)
            .unwrap_or_else(|error| panic!("{rust}: {error:#}\n{text}"));
    }
    for (rust, message) in [
        (
            "Option<()>",
            "`Option<()>` has no WIT equivalent, WIT has no empty value for an option to hold: use `bool`",
        ),
        ("Vec<()>", "`()` has no WIT equivalent as a value"),
        ("(u32, ())", "`()` has no WIT equivalent as a value"),
        (
            "Result<u32>",
            "the macro reads `Result` by its name as `Result<T, E>`",
        ),
    ] {
        let error = refused(PACKAGE, &method(rust));
        assert!(error.contains(message), "{rust}: {error}");
    }
}

/// WIT spells an option of an option, but the runtime refuses it in a signature: `Option<Option<T>>`
/// is refused where it is written, in a function and in a field of a type that enters the WIT,
/// while an option of another type expands.
#[test]
fn an_option_of_an_option_is_refused() {
    let module = |result: &str, field: &str| {
        format!(
            "mod book {{
                pub struct Found {{ pub at: {field} }}
                #[service]
                pub struct Ledger;
                impl Ledger {{
                    pub fn find(key: String) -> {result} {{ todo!() }}
                    pub fn last() -> Found {{ todo!() }}
                }}
            }}"
        )
    };
    let text = wit(&expanded(PACKAGE, &module("Option<u32>", "Option<u32>")));
    for expected in [
        "  record found {\n    at: option<u32>,\n  }\n",
        "  find: func(key: string) -> option<u32>;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    for (result, field) in [
        ("Option<Option<u32>>", "Option<u32>"),
        ("Option<u32>", "Option<Option<u32>>"),
    ] {
        let error = refused(PACKAGE, &module(result, field));
        assert!(
            error.contains("`Option<Option<T>>` is an option of an option, which the runtime refuses in a signature: use an enum of the module with a case for each"),
            "{result}, {field}: {error}"
        );
    }
}

/// The package, and every identity an attribute names, of an implemented interface, a hook or an
/// import, is a WIT identity: names of kebab-case words of lowercase letters and digits, each word
/// starting with a letter and no name a WIT keyword, and a semantic version when there is one.
/// Anything else is an error on the literal, never a panic or a line of foreign WIT.
#[test]
fn the_identities_on_attributes_are_wit_identities() {
    for (package, problem) in [
        ("example:", "the package name `` is empty"),
        ("example:@1.0.0", "the package name `` is empty"),
        (":greeter@1.0.0", "the namespace `` is empty"),
        (
            "acme:2fa@1.0.0",
            "the package name `2fa` has a word that does not start with a lowercase letter",
        ),
        (
            "acme:two--factor@1.0.0",
            "the package name `two--factor` has an empty word around a `-`",
        ),
        (
            "Acme:greeter@1.0.0",
            "the namespace `Acme` has a word that does not start with a lowercase letter",
        ),
        (
            "acme:greeter_two@1.0.0",
            "the package name `greeter_two` is not kebab-case words of lowercase letters and digits",
        ),
        (
            "acme:list@1.0.0",
            "the package name `list` is a WIT keyword",
        ),
        (
            "acme:greeter@1.0",
            "the version `1.0` is not a semantic version",
        ),
        (
            "acme:greeter@01.0.0",
            "the version `01.0.0` is not a semantic version",
        ),
        (
            "acme:greeter@1.0.0; interface injected {}",
            "the version `1.0.0; interface injected {}` is not a semantic version",
        ),
        (
            "greeter",
            "it has no `:` between the namespace and the package name",
        ),
    ] {
        let error = refused(package, &ledger("#[service]"));
        assert!(
            error.contains(&format!(
                "the package is named namespace:name@version, such as `example:name@1.0.0`; `{package}` is not: {problem}"
            )),
            "{package}: {error}"
        );
    }
    for package in [
        "example:book@1.0.0-rc.1+build.5",
        "example:book",
        "example:book2-v3@0.1.0",
    ] {
        let module = expanded(package, &ledger("#[service]"));
        assert!(
            wit(&module).starts_with(&format!("package {package};\n")),
            "{}",
            wit(&module)
        );
    }
    // The bindings escape a namespace that is a Rust keyword, `self_`, but not a package name.
    let module = expanded("self:book@1.0.0", &ledger("#[service]"));
    assert!(
        tokens(&module).contains("impl exports :: self_ :: book :: ledger :: Guest for"),
        "{}",
        tokens(&module)
    );
    let error = refused("example:self@1.0.0", &ledger("#[service]"));
    assert!(
        error.contains(
            "the package name `self` is a Rust keyword, which the bindings cannot name a module after"
        ),
        "{error}"
    );
    let error = refused(
        PACKAGE,
        &ledger("#[service(\"example:ledger/book@1.0.0;\\nworld injected {}\")]"),
    );
    assert!(
        error.contains("an implemented interface is named namespace:name/interface@version, such as `example:name/interface@1.0.0`"),
        "{error}"
    );
    let error = refused(PACKAGE, &ledger("#[service(\"example:ledger@1.0.0\")]"));
    assert!(
        error.contains("it names no interface after a `/`"),
        "{error}"
    );
    let error = refused(
        PACKAGE,
        &ledger("#[service]").replace(
            "pub struct Ledger;",
            "pub struct Ledger;\n#[hook(\"example:admission/Guard@1.0.0\", priority = 1)]\npub fn block(input: u32) -> u32 { input }",
        ),
    );
    assert!(
        error.contains("a hook is named namespace:name/interface@version")
            && error.contains("the interface name `Guard` has a word that does not start"),
        "{error}"
    );
    let error = refused(
        PACKAGE,
        &ledger("#[service]").replace(
            "pub struct Ledger;",
            "pub struct Ledger;\n#[hook(\"admission/guard@1.0.0\", composition = series)]\npub struct Guard;",
        ),
    );
    assert!(
        error.contains("a hook is named namespace:name/interface@version")
            && error.contains("it has no `:`"),
        "{error}"
    );
    let error = refused_with(
        Options {
            imports: vec![literal("example:ledger/book@1.0.0;\n  export injected;")],
            ..options(PACKAGE, None)
        },
        &ledger("#[service]"),
    );
    assert!(
        error.contains("an imported interface is named namespace:name/interface@version"),
        "{error}"
    );
}

const LEDGER_WIT: &str = "package example:ledger@1.0.0;
interface book {
  record entry { label: string, amount: s64 }
  add: func(entry: entry) -> u32;
  total: func() -> s64;
}
world keeper { export book; }
";

const KEEPER: &str = "mod keeper {
    #[service(\"example:ledger/book@1.0.0\")]
    pub struct Keeper;
    impl Keeper {
        pub fn add(entry: book::Entry) -> u32 { 0 }
        pub fn total() -> i64 { 0 }
    }
}";

/// An implemented interface is exported by its identity, never derived, and its generated module
/// is re-exported under its name; the `impl` has exactly the interface's functions.
#[test]
fn a_service_may_implement_a_consumed_interface() {
    let dir = consumed("keeper", LEDGER_WIT);
    let module = orb_with(options("example:keeper@1.0.0", Some(&dir)), KEEPER)
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        wit(&module),
        "package example:keeper@1.0.0;\n\nworld keeper {\n  export example:ledger/book@1.0.0;\n}\n"
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("pub use exports :: example :: ledger :: book ;"),
        "{generated}"
    );
    assert!(
        generated
            .contains("impl exports :: example :: ledger :: book :: Guest for __TembraExports"),
        "{generated}"
    );
    assert!(
        generated.contains("fn add (entry : exports :: example :: ledger :: book :: Entry) -> u32 { Keeper :: add (entry) }"),
        "{generated}"
    );
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        &KEEPER.replace("pub fn total() -> i64 { 0 }", ""),
    );
    assert!(error.contains("has no `pub fn total`"), "{error}");
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        &KEEPER.replace(
            "pub fn total() -> i64 { 0 }",
            "pub fn total() -> i64 { 0 } pub fn extra() {}",
        ),
    );
    assert!(error.contains("is not a function of"), "{error}");
    let error = refused("example:keeper@1.0.0", KEEPER);
    assert!(error.contains("needs wit ="), "{error}");
}

/// Each service gets the signature of its interface as a hidden constant: the digest the kernel
/// computes when it installs a provider, which `plan` gives the service for the derived WIT, so a
/// native Orb of the module declares it and installs. The implemented interface of a consumed
/// package keeps the signature its own contract gives it, with the consumed types shaping it.
#[test]
fn a_service_signature_is_the_one_the_kernel_computes() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub struct Entry { pub label: String, pub amount: i64 }
            pub enum Refusal { Closed, Over(u32) }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn add(entry: Entry) -> Result<u32, Refusal> { Ok(0) }
            }
        }",
    );
    let plan = tembra_contracts::Contract::from_source("book.wit", &wit(&module), Some("book"))
        .unwrap()
        .plan("book", 1, &Default::default())
        .unwrap();
    assert_eq!(plan.orb.services[0].name, "book::example:book/ledger@1.0.0");
    assert_eq!(
        constant(&module, "__TEMBRA_SIGNATURE_LEDGER"),
        plan.orb.services[0].signature.clone().unwrap()
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("# [doc (hidden)] pub const __TEMBRA_SIGNATURE_LEDGER : & str ="),
        "{generated}"
    );

    let dir = consumed("signature", LEDGER_WIT);
    let module = orb_with(options("example:keeper@1.0.0", Some(&dir)), KEEPER)
        .unwrap_or_else(|error| panic!("{error}"));
    let provider = tembra_contracts::Contract::from_dir(&dir, Some("keeper"))
        .unwrap()
        .plan("ledger", 1, &Default::default())
        .unwrap();
    assert_eq!(
        constant(&module, "__TEMBRA_SIGNATURE_KEEPER"),
        provider.orb.services[0].signature.clone().unwrap()
    );
}

/// Every reference to a service object has the members `then`, `release`, `state`, `ready`,
/// `status` and `opaque`, so the kernel refuses a method of one of those names: the macro refuses
/// it first, by the name the kernel compares, for a derived and for an implemented interface.
#[test]
fn a_service_method_may_not_take_a_reserved_member_name() {
    for name in ["then", "release", "state", "ready", "status", "opaque"] {
        let error = refused(
            PACKAGE,
            &ledger("#[service]").replace("pub fn count()", &format!("pub fn {name}()")),
        );
        assert!(
            error.contains(&format!(
                "`Ledger::{name}` is a method named `{name}`, a member every reference to a service object has"
            )),
            "{name}: {error}"
        );
    }
    // The kernel compares the lowerCamelCase spelling, which `then_` shares.
    let error = refused(
        PACKAGE,
        &ledger("#[service]").replace("pub fn count()", "pub fn then_()"),
    );
    assert!(error.contains("is a method named `then`"), "{error}");
    // A name that only contains one is a method like any other.
    let module = expanded(
        PACKAGE,
        &ledger("#[service]").replace("pub fn count()", "pub fn ready_count()"),
    );
    assert!(
        wit(&module).contains("ready-count: func() -> u32;"),
        "{}",
        wit(&module)
    );
    let dir = consumed(
        "reserved",
        &LEDGER_WIT.replace("total: func() -> s64;", "status: func() -> s64;"),
    );
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        &KEEPER.replace("pub fn total()", "pub fn status()"),
    );
    assert!(
        error.contains("example:ledger/book@1.0.0 has a function `status`, a member every reference to a service object has as `status`"),
        "{error}"
    );
}

const ENTRIES_WIT: &str = "package example:ledger@1.0.0;
interface book {
  resource entry {
    constructor(label: string);
    label: func() -> string;
    blank: static func() -> entry;
  }
  open: func(label: string) -> entry;
}
";

/// The resource of an implemented interface is bridged in its struct's own module, as a derived
/// one is: there the author's constructor, methods and static functions are visible whatever
/// their visibility, so the bridge calls them, never the trait's function of the same name. Its
/// native side is there too, which the service's registration calls by its path.
#[test]
fn an_implemented_resource_is_bridged_in_its_own_module() {
    let dir = consumed("entries", ENTRIES_WIT);
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            pub mod model {
                pub mod entries {
                    #[wit_resource(service = Keeper)]
                    pub struct Entry { label: String }
                    impl Entry {
                        #[constructor]
                        fn new(label: String) -> Self { Entry { label } }
                        pub fn label(&self) -> String { self.label.clone() }
                        pub fn blank() -> Entry { Entry::new(String::new()) }
                    }
                }
            }
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {
                pub fn open(label: String) -> model::entries::Entry { todo!() }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    for expected in [
        "impl exports :: example :: ledger :: book :: Guest for __TembraExports { type Entry = model :: entries :: Entry ;",
        "pub mod entries { pub struct Entry",
        "} impl super :: super :: exports :: example :: ledger :: book :: GuestEntry for Entry {",
        "extern crate alloc as __tembra_alloc ; :: tembra :: __host ! { impl :: tembra :: portable :: Issuable for Entry {",
        "} } # [cfg_attr (not (target_arch = \"wasm32\") , allow (dead_code))] struct __TembraExports ;",
        "model :: entries :: __tembra_resource_entry (:: tembra :: Service :: new (",
        "fn new (label : __tembra_alloc :: string :: String) -> Self { Entry :: new (label) }",
        "fn label (& self ,) -> __tembra_alloc :: string :: String { Entry :: label (self ,) }",
        "fn blank () -> super :: super :: exports :: example :: ledger :: book :: Entry { super :: super :: exports :: example :: ledger :: book :: Entry :: new (Entry :: blank ()) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

const CURSOR_WIT: &str = "package example:ledger@1.0.0;
interface book {
  record entry { label: string, amount: s64 }
  resource cursor {
    constructor();
    current: func() -> entry;
    all: func() -> list<entry>;
    merge: func(other: cursor) -> u32;
  }
  open: func() -> cursor;
}
";

/// The resource of an implemented interface speaks the consumed package's types: its functions
/// reach none of the module's, so a struct of the module named like a consumed record stays the
/// author's, out of the WIT, and the resource takes what its WIT says, an owned handle included.
#[test]
fn an_implemented_resource_reaches_none_of_the_module_types() {
    let dir = consumed("cursor", CURSOR_WIT);
    let module = |extra: &str| {
        format!(
            "mod keeper {{
                {extra}
                #[service(\"example:ledger/book@1.0.0\")]
                pub struct Keeper;
                impl Keeper {{ pub fn open() -> Cursor {{ Cursor::start() }} }}
                #[wit_resource(service = Keeper)]
                pub struct Cursor {{ at: u32 }}
                impl Cursor {{
                    #[constructor]
                    pub fn start() -> Self {{ Cursor {{ at: 0 }} }}
                    pub fn current(&self) -> book::Entry {{ todo!() }}
                    pub fn all(&self) -> Vec<book::Entry> {{ todo!() }}
                    pub fn merge(&self, other: book::Cursor) -> u32 {{ self.at }}
                }}
            }}"
        )
    };
    let alone = orb_with(options("example:keeper@1.0.0", Some(&dir)), &module(""))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        wit(&alone),
        "package example:keeper@1.0.0;\n\nworld keeper {\n  export example:ledger/book@1.0.0;\n}\n"
    );
    for (extra, kept) in [
        (
            "pub struct Entry { pub note: core::cell::Cell<u32> }",
            "pub struct Entry { pub note : core :: cell :: Cell < u32 > }",
        ),
        (
            "pub struct Entry { pub note: String }
            #[service]
            pub struct Notes;
            impl Notes { pub fn count() -> u32 { 0 } }",
            "pub struct Entry { pub note : String }",
        ),
    ] {
        let module = orb_with(options("example:keeper@1.0.0", Some(&dir)), &module(extra))
            .unwrap_or_else(|error| panic!("{extra}: {error}"));
        let text = wit(&module);
        assert!(!text.contains("record"), "{extra}: {text}");
        let generated = tokens(&module);
        assert!(generated.contains(kept), "{kept}\n{generated}");
        assert!(!generated.contains(":: { Entry }"), "{generated}");
    }
}

/// `import = "..."` on the attribute imports the interface in the world and re-exports its module.
#[test]
fn an_import_enters_the_world_and_is_reexported() {
    let dir = consumed("reader", LEDGER_WIT);
    let module = orb_with(
        Options {
            imports: vec![literal("example:ledger/book@1.0.0")],
            ..options("example:reader@1.0.0", Some(&dir))
        },
        &ledger("#[service]"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        wit(&module)
            .contains("world book {\n  import example:ledger/book@1.0.0;\n  export ledger;\n}"),
        "{}",
        wit(&module)
    );
    assert!(tokens(&module).contains("pub use self :: example :: ledger :: book ;"));
}

/// A Rust name whose WIT name is a keyword of WIT, `list`, `record` or `interface`, is written with
/// WIT's `%` escape wherever the WIT names it, and the bindings are asked for the world the same
/// way; a raw identifier is read without its `r#`.
#[test]
fn a_name_that_is_a_wit_keyword_is_escaped() {
    let module = expanded(
        "example:keywords@1.0.0",
        "mod list {
            pub struct Record { pub result: u32, pub from: Kind }
            pub enum Kind { Record, Variant }
            #[wit_resource]
            pub struct Stream { at: u32 }
            impl Stream {
                #[constructor]
                pub fn new(with: u32) -> Self { Stream { at: with } }
                pub fn own(&self) -> u32 { self.at }
                pub fn tuple() -> Stream { Stream { at: 0 } }
            }
            #[service]
            pub struct Interface;
            impl Interface {
                pub fn list(from: u32, r#use: String) -> Vec<Record> { todo!() }
                pub fn future(at: u32) -> Stream { Stream::new(at) }
            }
        }",
    );
    assert_eq!(
        resolved(&module),
        "package example:keywords@1.0.0;\n\ninterface %interface {\n  record %record {\n    %result: u32,\n    %from: kind,\n  }\n  enum kind {\n    %record,\n    %variant,\n  }\n  resource %stream {\n    constructor(%with: u32);\n    %own: func() -> u32;\n    %tuple: static func() -> %stream;\n  }\n  %list: func(%from: u32, %use: string) -> list<%record>;\n  %future: func(at: u32) -> %stream;\n}\n\nworld %list {\n  export %interface;\n}\n"
    );
    assert!(
        tokens(&module).contains("world : \"%list\" ,"),
        "{}",
        tokens(&module)
    );
}

/// A name the bindings spell otherwise than the author is bridged with the bindings' spelling: a
/// function `GET` is `get` and `r#move` is `move_`, a resource `HTTPSession` is `HttpSession`,
/// and a record `HTTPRequest` is re-exported from `HttpRequest` under the author's name.
#[test]
fn a_name_the_bindings_spell_otherwise_is_bridged() {
    let module = expanded(
        "example:web@1.0.0",
        "mod web {
            pub struct HTTPRequest { pub path: String }
            #[wit_resource]
            pub struct HTTPSession { request: HTTPRequest }
            impl HTTPSession {
                #[constructor]
                pub fn open(path: String) -> Self { todo!() }
                pub fn URL(&self) -> String { todo!() }
                pub fn r#type(&self) -> u32 { 0 }
            }
            #[service]
            pub struct HTTPServer;
            impl HTTPServer {
                pub fn GET(r#type: u32) -> u32 { r#type }
                pub fn r#move(path: String) -> HTTPRequest { todo!() }
                pub fn start(request: HTTPRequest) -> Option<HTTPSession> { None }
            }
        }",
    );
    let text = resolved(&module);
    for expected in [
        "  record http-request {\n",
        "  resource http-session {\n",
        "    url: func() -> string;\n    %type: func() -> u32;\n",
        "  get: func(%type: u32) -> u32;\n  move: func(path: string) -> http-request;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    let interface = "exports :: example :: web :: http_server";
    for expected in [
        format!("pub use {interface} :: {{ HttpRequest as HTTPRequest }} ;"),
        "type HttpSession = HTTPSession ;".into(),
        "fn get (r#type : u32) -> u32 { HTTPServer :: GET (r#type) }".into(),
        "fn move_ (path : String) -> HTTPRequest { HTTPServer :: r#move (path) }".into(),
        format!(
            "fn start (request : HTTPRequest) -> Option < {interface} :: HttpSession > {{ HTTPServer :: start (request) . map (| item | {interface} :: HttpSession :: new (item)) }}"
        ),
        format!("impl {interface} :: GuestHttpSession for HTTPSession"),
        "fn url (& self ,) -> String { HTTPSession :: URL (self ,) }".into(),
        "fn type_ (& self ,) -> u32 { HTTPSession :: r#type (self ,) }".into(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
}

/// The generated type replaces the author's, but the bindings name its fields and cases from
/// their WIT names: a field or a case they would name otherwise is refused, with the name to use;
/// a type that stays out of the WIT keeps its names.
#[test]
fn a_field_or_a_case_the_bindings_name_otherwise_is_refused() {
    let module = |types: &str| {
        format!(
            "mod web {{
                {types}
                #[service]
                pub struct Server;
                impl Server {{ pub fn get() -> Request {{ todo!() }} }}
            }}"
        )
    };
    for (types, message) in [
        (
            "pub struct Request { pub userID: u32 }",
            "the field `Request::userID` enters the WIT as `user-id`, which the generated type replacing `Request` names `user_id`: name the field `user_id`",
        ),
        (
            "pub struct Request { pub r#type: u32 }",
            "the field `Request::r#type` enters the WIT as `type`, which the generated type replacing `Request` names `type_`: name the field `type_`",
        ),
        (
            "pub enum Request { HTTP, Https }",
            "the case `Request::HTTP` enters the WIT as `http`, which the generated type replacing `Request` names `Http`: name the case `Http`",
        ),
        (
            "pub struct Request { pub café: u32 }",
            "the field `Request::café` has no WIT name: its kebab-case form `café` holds a character other than ASCII letters, digits and `-`",
        ),
    ] {
        let error = refused("example:web@1.0.0", &module(types));
        assert!(error.contains(message), "{types}: {error}");
    }
    let module = expanded(
        "example:web@1.0.0",
        &module(
            "pub struct Request { pub type_: u32, pub kind: Kind }\npub enum Kind { Http, Https }\npub struct Kept { pub userID: u32 }",
        ),
    );
    assert!(
        resolved(&module).contains("  record request {\n    %type: u32,\n    kind: kind,\n  }\n"),
        "{}",
        wit(&module)
    );
    assert!(tokens(&module).contains("pub struct Kept { pub userID : u32 }"));
}

/// Types and functions share an interface's names in WIT, a resource's methods and static
/// functions share the resource's, and the interfaces share the package's: two items of one WIT
/// name in one scope are a compile error naming both.
#[test]
fn two_items_of_one_wit_name_in_one_scope_are_refused() {
    let module = |types: &str, functions: &str| {
        format!(
            "mod office {{
                {types}
                #[service]
                pub struct Desk;
                impl Desk {{ {functions} }}
            }}"
        )
    };
    for (types, functions, message) in [
        (
            "pub struct Stats { pub words: u32 }",
            "pub fn stats() -> Stats { todo!() }",
            "the type `Stats` and `Desk::stats` are both `stats` in the interface `desk`, where types and functions share one namespace; rename one of them",
        ),
        (
            "pub struct Info { pub n: u32 }\npub struct Report { pub info: Info }",
            "pub fn summary() -> Report { todo!() } pub fn info() -> u32 { 0 }",
            "the type `Info` and `Desk::info` are both `info` in the interface `desk`",
        ),
        (
            "",
            "pub fn r#move(by: u32) -> u32 { by } pub fn move_(by: u32) -> u32 { by }",
            "`Desk::r#move` and `Desk::move_` are both `move` in the interface `desk`",
        ),
        (
            "#[wit_resource]\npub struct Pen;\nimpl Pen { pub fn ink_level(&self) -> u32 { 0 } }",
            "pub fn pen() -> u32 { 0 }",
            "the resource `Pen` and `Desk::pen` are both `pen` in the interface `desk`",
        ),
        (
            "#[wit_resource]\npub struct Pen;\nimpl Pen { pub fn ink_level(&self) -> u32 { 0 } pub fn inkLevel() -> u32 { 0 } }",
            "pub fn take() -> Pen { Pen }",
            "`Pen::ink_level` and `Pen::inkLevel` are both `ink-level` in the resource `pen`, where its methods and static functions share one namespace",
        ),
        (
            "",
            "pub fn add(by_count: u32, byCount: u32) -> u32 { 0 }",
            "the parameter `by_count` and the parameter `byCount` are both `by-count` in the parameters of `Desk::add`",
        ),
        (
            "",
            "pub fn café() -> u32 { 0 }",
            "`Desk::café` has no WIT name: its kebab-case form `café` holds a character other than ASCII letters, digits and `-`",
        ),
        (
            "#[service]\npub struct DESK;\nimpl DESK { pub fn count() -> u32 { 0 } }",
            "pub fn total() -> u32 { 0 }",
            "`DESK` and `Desk` are both the interface `desk` in the WIT; rename one of them",
        ),
    ] {
        let error = refused("example:office@1.0.0", &module(types, functions));
        assert!(error.contains(message), "{types} {functions}: {error}");
    }
}

/// An implemented interface or a consumed hook whose WIT names are Rust keywords is bridged as the
/// bindings name them, `move_`, `type_`, `return_`, and calls the author's functions by their own
/// names, matched by WIT name.
#[test]
fn a_consumed_name_that_is_a_rust_keyword_is_bridged_as_the_bindings_name_it() {
    let dir = consumed(
        "keywords",
        "package example:ledger@1.0.0;
        interface book {
          resource cursor {
            constructor(%type: u32);
            %match: func(%in: u32) -> bool;
            %static: static func(%as: u32) -> cursor;
          }
          %move: func(%type: u32) -> u32;
          get-entry: func() -> u32;
        }
        interface guard {
          record admission { value: s32 }
          %return: func(%in: admission) -> admission;
        }",
    );
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {
                pub fn r#move(r#type: u32) -> u32 { r#type }
                pub fn getEntry() -> u32 { 0 }
            }
            #[wit_resource(service = Keeper)]
            pub struct Cursor { at: u32 }
            impl Cursor {
                #[constructor]
                pub fn new(r#type: u32) -> Self { Cursor { at: r#type } }
                pub fn r#match(&self, r#in: u32) -> bool { self.at == r#in }
                pub fn static_(r#as: u32) -> Cursor { Cursor { at: r#as } }
            }
            #[hook(\"example:ledger/guard@1.0.0\", priority = 1)]
            pub fn admit(input: guard::Admission) -> guard::Admission { input }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    for expected in [
        "fn move_ (type_ : u32) -> u32 { Keeper :: r#move (type_) }",
        "fn get_entry () -> u32 { Keeper :: getEntry () }",
        "fn new (type_ : u32) -> Self { Cursor :: new (type_) }",
        "fn match_ (& self , in_ : u32) -> bool { Cursor :: r#match (self , in_) }",
        "fn static_ (as_ : u32) -> exports :: example :: ledger :: book :: Cursor { exports :: example :: ledger :: book :: Cursor :: new (Cursor :: static_ (as_)) }",
        "fn return_ (input : guard :: Admission) -> guard :: Admission { admit (input) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let declared = orb_with(
        options("example:gate@1.0.0", Some(&dir)),
        "mod gate {
            #[hook(\"example:ledger/guard@1.0.0\", composition = waterfall)]
            pub struct Guard;
            #[service]
            pub struct Admissions;
            impl Admissions { pub fn admit(value: i32) -> i32 { Guard::run(guard::Admission { value }).value } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&declared);
    for expected in [
        "fn return_ (_input : exports :: example :: ledger :: guard :: Admission)",
        "self :: example :: ledger :: guard :: return_ (input)",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {
                pub fn r#move(r#type: u32) -> u32 { r#type }
                pub fn move_(r#type: u32) -> u32 { r#type }
                pub fn get_entry() -> u32 { 0 }
            }
            #[wit_resource(service = Keeper)]
            pub struct Cursor;
        }",
    );
    assert!(
        error.contains("`r#move` and `move_` both implement the WIT function `move`"),
        "{error}"
    );
}

/// The derived WIT, checked to resolve as a package.
fn resolved(module: &ItemMod) -> String {
    let text = wit(module);
    resolved_with(wit_parser::Resolve::default(), &text);
    text
}

/// Check that `text`, a derived WIT, resolves after the packages `resolve` holds, as the bindings
/// read it.
pub(crate) fn resolved_with(mut resolve: wit_parser::Resolve, text: &str) {
    resolve
        .push_str("orb.wit", text)
        .unwrap_or_else(|error| panic!("{error:#}\n{text}"));
}

/// An interface `use`s what the constructors, methods and static functions of its own resources
/// name, as it does for its own functions: a module record, a type a derived hook defines. Another
/// service's resource they cannot name, which the runtime would refuse.
#[test]
fn an_interface_uses_what_its_resources_mention() {
    let record = resolved(&expanded(
        "example:wa@1.0.0",
        "mod wa {
            pub struct Info { pub n: u32 }
            #[service]
            pub struct Main;
            impl Main { pub fn ping() -> u32 { 1 } }
            #[wit_resource(service = Sessions)]
            pub struct Session;
            impl Session { pub fn info(&self) -> Info { Info { n: 1 } } }
            #[service]
            pub struct Sessions;
            impl Sessions { pub fn open() -> Session { Session } }
        }",
    ));
    assert!(
        record.contains("resource session {\n    info: func() -> info;\n  }"),
        "{record}"
    );
    let hook = resolved(&expanded(
        "example:wahook@1.0.0",
        "mod wahook {
            #[hook(composition = waterfall, input = Admission)]
            pub struct Guard;
            pub struct Admission { pub value: i32, pub blocked: bool }
            #[wit_resource]
            pub struct Ticket;
            impl Ticket {
                #[constructor]
                pub fn new(admission: Admission) -> Self { Ticket }
                pub fn admission(&self) -> Admission { todo!() }
            }
            #[service]
            pub struct Admissions;
            impl Admissions { pub fn issue() -> Ticket { Ticket } }
        }",
    ));
    assert!(
        hook.contains("interface admissions {\n  use guard.{admission};\n"),
        "{hook}"
    );
}

const FRONT: &str = "mod wb {
    /// Counts over the texts.
    pub struct Stats { pub words: u32 }
    #[service]
    pub struct Front;
    impl Front { pub fn start() -> Builder { Builder } }
    #[wit_resource(service = Texts)]
    pub struct Builder;
    impl Builder { pub fn text(&self) -> u32 { 0 } }
    #[service]
    pub struct Texts;
    impl Texts { pub fn counts() -> Stats { Stats { words: 0 } } }
}";

/// A type is defined in the interface of the earliest service that mentions it, through its
/// functions, its resources' functions or other types, and re-exported from there; a later service
/// mentioning it `use`s it. A resource stays with its own service: another one handing it out is
/// refused, as `a_resource_is_returned_only_from_its_own_interface` shows.
#[test]
fn a_type_is_defined_by_the_earliest_service_that_mentions_it() {
    let module = expanded(
        "example:wb@1.0.0",
        &FRONT.replace(
            "impl Front { pub fn start() -> Builder { Builder } }",
            "impl Front { pub fn summary() -> Stats { Stats { words: 0 } } }",
        ),
    );
    assert_eq!(
        resolved(&module),
        "package example:wb@1.0.0;\n\ninterface front {\n  /// Counts over the texts.\n  record stats {\n    words: u32,\n  }\n  summary: func() -> stats;\n}\n\ninterface texts {\n  use front.{stats};\n  resource builder {\n    text: func() -> u32;\n  }\n  counts: func() -> stats;\n}\n\nworld wb {\n  export front;\n  export texts;\n}\n"
    );
    assert!(
        tokens(&module).contains("pub use exports :: example :: wb :: front :: { Stats } ;"),
        "{}",
        tokens(&module)
    );
    // A type only a later service's resource mentions is that service's.
    let record = resolved(&expanded(
        "example:wa@1.0.0",
        "mod wa {
            pub struct Info { pub n: u32, pub detail: Detail }
            pub struct Detail { pub text: String }
            #[service]
            pub struct Main;
            impl Main { pub fn ping() -> u32 { 1 } }
            #[wit_resource(service = Sessions)]
            pub struct Session;
            impl Session { pub fn info(&self) -> Info { todo!() } }
            #[service]
            pub struct Sessions;
            impl Sessions { pub fn open() -> Session { Session } }
        }",
    ));
    assert!(
        record.contains("interface main {\n  ping: func() -> u32;\n}"),
        "{record}"
    );
    assert!(
        record.contains("interface sessions {\n  record info {\n    n: u32,\n    detail: detail,\n  }\n  record detail {\n"),
        "{record}"
    );
}

/// The runtime returns a resource only from the interface that defines it, and moves one only into
/// it: a derived function, a resource's method or a static function returning another interface's
/// resource, alone, nested or in a record of the module, is refused on its result type, and so is
/// a parameter whose record holds one, or a hook's input whose record holds a resource. The
/// interface that defines the resource returns it, in a record too.
#[test]
fn a_resource_is_returned_only_from_its_own_interface() {
    for (front, message) in [
        (
            "impl Front { pub fn start() -> Builder { Builder } }",
            "`Front::start` returns `Builder`, a resource of the interface `texts`: the runtime returns a resource only from the interface that defines it, so return it from a function of `Texts` or of one of its resources",
        ),
        (
            "impl Front { pub fn find() -> Result<Option<Builder>, String> { Ok(None) } }",
            "`Front::find` returns `Builder`, a resource of the interface `texts`",
        ),
        (
            "pub struct Lease { pub builder: Builder }
            impl Front { pub fn lend() -> Vec<Lease> { Vec::new() } }",
            "`Front::lend` returns `Lease`, which holds `Builder`, a resource of the interface `texts`",
        ),
        (
            "pub struct Lease { pub builder: Builder }
            impl Front { pub fn keep(lease: Lease) {} }",
            "`Front::keep` takes `Lease`, which holds `Builder`, a resource of the interface `texts`: the runtime moves an owned resource only into the interface that defines it, so take it in a function of `Texts` or of one of its resources",
        ),
        (
            "impl Front { pub fn ping() -> u32 { 0 } }
            #[wit_resource(service = Front)]
            pub struct Draft;
            impl Draft {
                pub fn text(&self) -> u32 { 0 }
                pub fn fork() -> (u32, Builder) { (0, Builder) }
            }",
            "`Draft::fork` returns `Builder`, a resource of the interface `texts`",
        ),
    ] {
        let error = refused(
            "example:wb@1.0.0",
            &FRONT.replace(
                "impl Front { pub fn start() -> Builder { Builder } }",
                front,
            ),
        );
        assert!(error.contains(message), "{front}: {error}");
    }
    let error = refused(
        "example:wa@1.0.0",
        "mod wa {
            #[wit_resource(service = Main)]
            pub struct Token;
            impl Token { pub fn id(&self) -> u32 { 7 } }
            #[service]
            pub struct Main;
            impl Main { pub fn issue() -> Token { Token } }
            #[wit_resource(service = Sessions)]
            pub struct Session;
            impl Session { pub fn grant(&self) -> Token { Token } }
            #[service]
            pub struct Sessions;
            impl Sessions { pub fn open() -> Session { Session } }
        }",
    );
    assert!(
        error.contains("`Session::grant` returns `Token`, a resource of the interface `main`"),
        "{error}"
    );
    let dir = consumed("returned-cursor", CURSOR_WIT);
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper { pub fn open() -> Cursor { Cursor::start() } }
            #[wit_resource(service = Keeper)]
            pub struct Cursor { at: u32 }
            impl Cursor {
                #[constructor]
                pub fn start() -> Self { Cursor { at: 0 } }
                pub fn current(&self) -> book::Entry { todo!() }
                pub fn all(&self) -> Vec<book::Entry> { todo!() }
                pub fn merge(&self, other: book::Cursor) -> u32 { self.at }
            }
            #[service]
            pub struct Tools;
            impl Tools { pub fn fresh() -> Option<Cursor> { None } }
        }",
    );
    assert!(
        error.contains("`Tools::fresh` returns `Cursor`, a resource of the implemented interface example:ledger/book@1.0.0: the runtime returns a resource only from the interface that defines it"),
        "{error}"
    );
    let error = refused(
        "example:wahook@1.0.0",
        "mod wahook {
            #[hook(composition = series, input = Admission)]
            pub struct Guard;
            pub struct Admission { pub ticket: Ticket }
            #[wit_resource]
            pub struct Ticket;
            impl Ticket { pub fn id(&self) -> u32 { 0 } }
            #[service]
            pub struct Admissions;
            impl Admissions { pub fn issue() -> Ticket { Ticket } }
        }",
    );
    assert!(
        error.contains("the input of the hook `Guard` holds `Admission`, which holds the resource `Ticket`: a hook's input holds no resource"),
        "{error}"
    );
    let own = resolved(&expanded(
        "example:wb@1.0.0",
        &FRONT
            .replace(
                "impl Texts { pub fn counts() -> Stats { Stats { words: 0 } } }",
                "pub struct Lease { pub builder: Builder }
            impl Texts {
                pub fn counts() -> Stats { Stats { words: 0 } }
                pub fn lend() -> Option<Lease> { None }
            }",
            )
            .replace(
                "impl Front { pub fn start() -> Builder { Builder } }",
                "impl Front { pub fn ping() -> u32 { 0 } }",
            ),
    ));
    assert!(own.contains("  lend: func() -> option<lease>;\n"), "{own}");
}

const DESK: &str = "mod desk {
    /// One session.
    #[wit_resource]
    pub struct Session { n: u32 }
    impl Session {
        #[constructor]
        pub fn new(n: u32) -> Session { Session { n } }
        /// The number.
        pub fn get(&self) -> u32 { self.n }
        pub fn twin(&self) -> Session { Session { n: self.n } }
        pub fn make() -> Session { Session { n: 0 } }
    }
    #[service]
    pub struct Sessions;
    impl Sessions {
        pub fn open(n: u32) -> Session { Session::new(n) }
    }
}";

/// A `#[wit_resource]` struct is a `resource` of its service's interface, with its constructor,
/// methods and static functions; the bridges wrap the author's values into handles.
#[test]
fn a_wit_resource_is_derived_from_the_struct_and_its_methods() {
    let module = expanded("example:desk@1.0.0", DESK);
    assert_eq!(
        wit(&module),
        "package example:desk@1.0.0;\n\ninterface sessions {\n  /// One session.\n  resource session {\n    constructor(n: u32);\n    /// The number.\n    get: func() -> u32;\n    twin: func() -> session;\n    make: static func() -> session;\n  }\n  open: func(n: u32) -> session;\n}\n\nworld desk {\n  export sessions;\n}\n"
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("type Session = Session ;"),
        "{generated}"
    );
    assert!(
        generated
            .contains("impl exports :: example :: desk :: sessions :: GuestSession for Session"),
        "{generated}"
    );
    assert!(
        generated.contains("fn open (n : u32) -> exports :: example :: desk :: sessions :: Session { exports :: example :: desk :: sessions :: Session :: new (Sessions :: open (n)) }"),
        "{generated}"
    );
    assert!(
        generated.contains("fn get (& self ,) -> u32 { Session :: get (self ,) }"),
        "{generated}"
    );
    assert!(
        generated.contains("fn new (n : u32) -> Self { Session :: new (n) }"),
        "{generated}"
    );
    let error = refused(
        "example:desk@1.0.0",
        &DESK.replace("pub fn get(&self) -> u32", "pub fn get(&mut self) -> u32"),
    );
    assert!(error.contains("borrows `&self`"), "{error}");
}

/// A resource inside an option, a result, a list or a tuple is wrapped where it is: the bridge
/// spells the bindings' type with the handle in the resource's place and rebuilds the containers
/// around it; a value holding no resource crosses as it is.
#[test]
fn a_nested_resource_is_wrapped_where_it_is() {
    let module = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn find(n: u32) -> Option<Session> { None }
            pub fn checked(n: u32) -> Result<Session, String> { todo!() }
            pub fn refused(n: u32) -> Result<u32, Session> { todo!() }
            pub fn many(n: u32) -> Vec<Session> { todo!() }
            pub fn counted(n: u32) -> (Session, u32) { todo!() }
            pub fn plain(n: u32) -> Option<u32> { None }",
        ),
    );
    let module_path = "exports :: example :: desk :: sessions";
    let generated = tokens(&module);
    for expected in [
        format!(
            "fn find (n : u32) -> Option < {module_path} :: Session > {{ Sessions :: find (n) . map (| item | {module_path} :: Session :: new (item)) }}"
        ),
        format!(
            "fn checked (n : u32) -> Result < {module_path} :: Session , String > {{ match Sessions :: checked (n) {{ :: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok ({module_path} :: Session :: new (item)) , :: core :: result :: Result :: Err (item) => :: core :: result :: Result :: Err (item) , }} }}"
        ),
        format!(
            "fn refused (n : u32) -> Result < u32 , {module_path} :: Session > {{ match Sessions :: refused (n) {{ :: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok (item) , :: core :: result :: Result :: Err (item) => :: core :: result :: Result :: Err ({module_path} :: Session :: new (item)) , }} }}"
        ),
        format!(
            "fn many (n : u32) -> Vec < {module_path} :: Session > {{ Sessions :: many (n) . into_iter () . map (| item | {module_path} :: Session :: new (item)) . collect () }}"
        ),
        format!(
            "fn counted (n : u32) -> ({module_path} :: Session , u32 ,) {{ {{ let (item0 , item1 ,) = Sessions :: counted (n) ; ({module_path} :: Session :: new (item0) , item1 ,) }} }}"
        ),
        "fn plain (n : u32) -> Option < u32 > { Sessions :: plain (n) }".into(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    // A resource's own method wraps the same way.
    let twin = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn twin(&self) -> Session { Session { n: self.n } }",
            "pub fn twin(&self) -> Option<Session> { None }",
        ),
    );
    assert!(
        tokens(&twin).contains(&format!(
            "fn twin (& self ,) -> Option < {module_path} :: Session > {{ Session :: twin (self ,) . map (| item | {module_path} :: Session :: new (item)) }}"
        )),
        "{}",
        tokens(&twin)
    );
}

/// A derived function takes a resource of its own interface, owned, alone or inside an option, a
/// result, a list or a tuple, and so do the resource's constructor, methods and static functions:
/// the WIT names the resource where the author's type does, and the bridge takes each handle back
/// into the author's value, which consumes it.
#[test]
fn a_derived_function_takes_a_resource_of_its_interface() {
    let module = expanded(
        "example:desk@1.0.0",
        &DESK
            .replace(
                "pub fn open(n: u32) -> Session { Session::new(n) }",
                "pub fn open(n: u32) -> Session { Session::new(n) }
                pub fn close(session: Session) -> u32 { session.n }
                pub fn close_all(sessions: Vec<Session>, last: Option<Session>) -> u32 { 0 }
                pub fn settle(outcome: Result<Session, String>, pair: (Session, u32)) {}
                pub async fn later(session: Session) -> u32 { session.n }",
            )
            .replace(
                "pub fn new(n: u32) -> Session { Session { n } }",
                "pub fn new(n: u32, from: Option<Session>) -> Session { Session { n } }
                pub fn merge(&self, other: Session) -> u32 { self.n + other.n }
                pub fn adopt(other: Session) -> Session { other }",
            ),
    );
    let text = resolved(&module);
    for expected in [
        "    constructor(n: u32, %from: option<session>);\n",
        "    merge: func(other: session) -> u32;\n",
        "    adopt: static func(other: session) -> session;\n",
        "  close: func(session: session) -> u32;\n",
        "  close-all: func(sessions: list<session>, last: option<session>) -> u32;\n",
        "  settle: func(outcome: result<session, string>, pair: tuple<session, u32>);\n",
        "  later: async func(session: session) -> u32;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let handle = "exports :: example :: desk :: sessions :: Session";
    let generated = tokens(&module);
    for expected in [
        format!(
            "fn close (session : {handle}) -> u32 {{ Sessions :: close (session . into_inner ()) }}"
        ),
        format!(
            "fn close_all (sessions : Vec < {handle} > , last : Option < {handle} >) -> u32 {{ Sessions :: close_all (sessions . into_iter () . map (| item | item . into_inner ()) . collect () , last . map (| item | item . into_inner ())) }}"
        ),
        format!(
            "fn settle (outcome : Result < {handle} , String > , pair : ({handle} , u32 ,)) {{ Sessions :: settle (match outcome {{ :: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok (item . into_inner ()) , :: core :: result :: Result :: Err (item) => :: core :: result :: Result :: Err (item) , }} , {{ let (item0 , item1 ,) = pair ; (item0 . into_inner () , item1 ,) }}) }}"
        ),
        format!(
            "async fn later (session : {handle}) -> u32 {{ Sessions :: later (session . into_inner ()) . await }}"
        ),
        format!(
            "fn new (n : u32 , from : Option < {handle} >) -> Self {{ Session :: new (n , from . map (| item | item . into_inner ())) }}"
        ),
        format!(
            "fn merge (& self , other : {handle}) -> u32 {{ Session :: merge (self , other . into_inner ()) }}"
        ),
        format!(
            "fn adopt (other : {handle}) -> {handle} {{ {handle} :: new (Session :: adopt (other . into_inner ())) }}"
        ),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
}

/// The runtime moves an owned resource only into the interface that defines it, and carries no
/// borrowed argument between Orbs: a derived function taking a resource of another service's
/// interface, a resource of an implemented interface or a borrowed resource is refused, and so is
/// a derived hook whose input holds a resource.
#[test]
fn a_resource_the_runtime_cannot_carry_into_a_function_is_refused() {
    let error = refused(
        "example:wb@1.0.0",
        &FRONT.replace(
            "impl Front { pub fn start() -> Builder { Builder } }",
            "impl Front { pub fn start() -> Builder { Builder } pub fn take(builder: Option<Builder>) {} }",
        ),
    );
    assert!(
        error.contains("`Front::take` takes `Builder`, a resource of the interface `texts`: the runtime moves an owned resource only into the interface that defines it, so take it in a function of `Texts` or of one of its resources"),
        "{error}"
    );
    let error = refused(
        "example:wb@1.0.0",
        &FRONT.replace(
            "impl Texts { pub fn counts() -> Stats { Stats { words: 0 } } }",
            "impl Texts { pub fn counts() -> Stats { Stats { words: 0 } } }
            #[wit_resource(service = Front)]
            pub struct Draft;
            impl Draft { pub fn keep(&self, builder: Builder) {} }",
        ),
    );
    assert!(
        error.contains("`Draft::keep` takes `Builder`, a resource of the interface `texts`"),
        "{error}"
    );
    let error = refused(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) } pub fn peek(session: &Session) -> u32 { 0 }",
        ),
    );
    assert!(
        error.contains("WIT boundaries pass owned values"),
        "{error}"
    );
    let dir = consumed("taken-cursor", CURSOR_WIT);
    let error = refused_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper { pub fn open() -> Cursor { Cursor::start() } }
            #[wit_resource(service = Keeper)]
            pub struct Cursor { at: u32 }
            impl Cursor {
                #[constructor]
                pub fn start() -> Self { Cursor { at: 0 } }
                pub fn current(&self) -> book::Entry { todo!() }
                pub fn all(&self) -> Vec<book::Entry> { todo!() }
                pub fn merge(&self, other: book::Cursor) -> u32 { self.at }
            }
            #[service]
            pub struct Tools;
            impl Tools { pub fn measure(cursor: Cursor) -> u32 { 0 } }
        }",
    );
    assert!(
        error.contains("`Tools::measure` takes `Cursor`, a resource of the implemented interface example:ledger/book@1.0.0"),
        "{error}"
    );
    let error = refused(
        "example:wahook@1.0.0",
        "mod wahook {
            #[hook(composition = series, input = Ticket)]
            pub struct Guard;
            #[wit_resource]
            pub struct Ticket;
            impl Ticket { pub fn id(&self) -> u32 { 0 } }
            #[service]
            pub struct Admissions;
            impl Admissions { pub fn issue() -> Ticket { Ticket } }
        }",
    );
    assert!(
        error.contains("the input of the hook `Guard` holds the resource `Ticket`: a hook's input holds no resource"),
        "{error}"
    );
    let error = refused_with(
        Options {
            imports: vec![literal("wasi:filesystem/types@0.3.0")],
            ..options("example:files@1.0.0", None)
        },
        "mod storage {
            #[service]
            pub struct Files;
            impl Files { pub fn size(file: types::Descriptor) -> u64 { 0 } }
        }",
    );
    assert!(
        error.contains("`Files::size` takes `Descriptor`, a resource of the WASI interface wasi:filesystem/types@0.3.0: the runtime moves no WASI handle between Orbs"),
        "{error}"
    );
}

const TEXT_WIT: &str = "package example:text@1.0.0;
interface texts {
  resource builder {
    constructor();
    text: func() -> string;
  }
  resource a-b;
  start: func() -> builder;
}
";

/// An imported interface's handle, named through the interface's module, is a parameter of a
/// derived function, alone or in a container: the derived interface `use`s the resource from the
/// imported one, under the kebab-case name of its handle, and the bridge passes the handle on as
/// it is, the bindings' own type.
#[test]
fn a_derived_function_takes_an_imported_handle() {
    let dir = consumed("speller", TEXT_WIT);
    let importing = || Options {
        imports: vec![literal("example:text/texts@1.0.0")],
        ..options("example:speller@1.0.0", Some(&dir))
    };
    let module = orb_with(
        importing(),
        "mod speller {
            #[wit_resource]
            pub struct Note { text: String }
            impl Note {
                #[constructor]
                pub fn new(builder: texts::Builder) -> Self { Note { text: builder.text() } }
                pub fn append(&self, more: Option<texts::Builder>) -> u32 { 0 }
            }
            #[service]
            pub struct Spelling;
            impl Spelling {
                pub fn finish(builder: texts::Builder) -> String { builder.text() }
                pub fn count(builders: Vec<texts::Builder>, odd: texts::AB) -> u32 { 0 }
                pub fn jot(builder: texts::Builder) -> Note { Note::new(builder) }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let text = wit(&module);
    for expected in [
        "interface spelling {\n  use example:text/texts@1.0.0.{builder, a-b as ab};\n",
        "    constructor(builder: builder);\n",
        "    append: func(more: option<builder>) -> u32;\n",
        "  finish: func(builder: builder) -> string;\n",
        "  count: func(builders: list<builder>, odd: ab) -> u32;\n",
        "world speller {\n  import example:text/texts@1.0.0;\n  export spelling;\n}\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_dir(&dir).unwrap();
    resolve
        .push_str("orb.wit", &text)
        .unwrap_or_else(|error| panic!("{error:#}\n{text}"));
    let generated = tokens(&module);
    for expected in [
        "fn finish (builder : texts :: Builder) -> String { Spelling :: finish (builder) }",
        "fn count (builders : Vec < texts :: Builder > , odd : texts :: AB) -> u32 { Spelling :: count (builders , odd) }",
        "fn new (builder : texts :: Builder) -> Self { Note :: new (builder) }",
        "fn append (& self , more : Option < texts :: Builder >) -> u32 { Note :: append (self , more) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let refused = |module: &str| refused_with(importing(), module);
    let error = refused(
        "mod speller {
            #[service]
            pub struct Spelling;
            impl Spelling { pub fn begin() -> Option<texts::Builder> { None } }
        }",
    );
    assert!(
        error.contains("`Spelling::begin` returns `Builder`, a resource of the imported interface example:text/texts@1.0.0: the runtime returns a resource only from the interface that defines it"),
        "{error}"
    );
    let error = refused(
        "mod speller {
            #[service]
            pub struct Spelling;
            impl Spelling { pub fn finish(builder: texts::Bulder) -> u32 { 0 } }
        }",
    );
    assert!(
        error.contains("`Spelling::finish` takes `Bulder`, which names no type or resource of the imported interface example:text/texts@1.0.0: its resources are `Builder`, `AB`"),
        "{error}"
    );
    // A type of the module named like the handle clashes in an interface that knows both, and
    // nowhere else: the bridges name the handle by its path.
    let named_like = |draft: &str| {
        format!(
            "mod speller {{
                #[wit_resource(service = Drafts)]
                pub struct Builder {{ pub text: String }}
                #[service]
                pub struct Spelling;
                impl Spelling {{
                    pub fn finish(builder: texts::Builder) -> u32 {{ 0 }}
                }}
                #[service]
                pub struct Drafts;
                impl Drafts {{ {draft} }}
            }}"
        )
    };
    let error = refused(&named_like(
        "pub fn draft() -> Builder { todo!() } pub fn finish(builder: texts::Builder) -> u32 { 0 }",
    ));
    assert!(
        error.contains("`Builder`, used from example:text/texts@1.0.0, and the resource `Builder` are both `builder` in the interface `drafts`"),
        "{error}"
    );
    let module = orb_with(
        importing(),
        &named_like("pub fn draft() -> Builder { todo!() }"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    for expected in [
        "fn finish (builder : texts :: Builder) -> u32 { Spelling :: finish (builder) }",
        "fn draft () -> exports :: example :: speller :: drafts :: Builder { exports :: example :: speller :: drafts :: Builder :: new (Drafts :: draft ()) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

const PERMITS: &str = "mod office {
    /// What a holder may do.
    #[flags]
    #[derive(Default, PartialEq, Clone)]
    pub struct Access { pub read: bool, pub write: bool, pub r#type: bool }
    pub struct Grant { pub who: String, pub access: Access }
    #[service]
    pub struct Permits;
    impl Permits {
        pub fn invert(access: Access) -> Access { todo!() }
        pub fn merged(grants: Vec<Grant>, extra: Option<Access>) -> Grant { todo!() }
    }
}";

/// A struct of `bool` fields marked `#[flags]` is a WIT `flags`, a flag per field in order, and
/// stays the author's: the bindings take it through `with`, so nothing re-exports a generated type
/// in its place, and the macro gives it what the bindings call on it, the derives a generated
/// record holding it needs, and none the author wrote twice.
#[test]
fn a_flags_struct_is_a_wit_flags_the_author_keeps() {
    let module = expanded("example:office@1.0.0", PERMITS);
    let text = resolved(&module);
    for expected in [
        "  /// What a holder may do.\n  flags access {\n    read,\n    write,\n    %type,\n  }\n",
        "  record grant {\n    who: string,\n    access: access,\n  }\n",
        "  invert: func(access: access) -> access;\n",
        "  merged: func(grants: list<grant>, extra: option<access>) -> grant;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    for expected in [
        "with : { \"example:office/permits@1.0.0/access\" : super :: super :: super :: super :: Access } ,",
        "pub use exports :: example :: office :: permits :: { Grant } ;",
        "# [derive (Default , PartialEq , Clone)] # [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access",
        "pub const fn empty () -> Self { Self { read : false , write : false , r#type : false } }",
        "pub const fn bits (& self) -> u32 { (self . read as u32) | ((self . write as u32) << 1) | ((self . r#type as u32) << 2) }",
        "pub const fn from_bits_retain (bits : u32) -> Self { Self { read : (bits & 1) != 0 , write : (bits & 2) != 0 , r#type : (bits & 4) != 0 } }",
        "impl :: core :: ops :: BitOr for Access",
        "Self { read : self . read | other . read , write : self . write | other . write , r#type : self . r#type | other . r#type }",
        "fn invert (access : Access) -> Access { Permits :: invert (access) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    // A trait the author implements by hand is not derived again.
    let module = expanded(
        "example:office@1.0.0",
        &PERMITS.replace(
            "pub struct Grant",
            "impl core::fmt::Debug for Access {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
            }
            pub struct Grant",
        ),
    );
    assert!(
        tokens(&module).contains("# [derive (:: core :: marker :: Copy)] pub struct Access"),
        "{}",
        tokens(&module)
    );
    // Nor is a trait a derive implements under another name, as `#[educe(..)]` names it.
    let module = expanded(
        "example:office@1.0.0",
        &PERMITS.replace(
            "#[derive(Default, PartialEq, Clone)]",
            "#[derive(Default, PartialEq, educe::Educe)] #[educe(Clone, Debug(name(Permit)))]",
        ),
    );
    assert!(
        tokens(&module).contains("# [derive (:: core :: marker :: Copy)] pub struct Access"),
        "{}",
        tokens(&module)
    );
}

/// In an Orb with a state the flags derive serde's traits, and a record holding the module's own
/// flags is no longer left out of the derives the state makes the bindings add: the flags are
/// the author's struct, serde's now.
#[test]
fn a_flags_struct_is_serde_in_an_orb_with_a_state() {
    let module = expanded(
        "example:office@1.0.0",
        &PERMITS.replace(
            "pub struct Grant",
            "#[state(service = Permits)]
            pub struct Granted { pub last: Access, pub grants: u32 }
            pub struct Grant",
        ),
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug , :: serde :: Serialize , :: serde :: Deserialize)] pub struct Access"),
        "{generated}"
    );
    assert!(
        generated
            .contains("additional_derives : [:: serde :: Serialize , :: serde :: Deserialize] ,"),
        "{generated}"
    );
    assert!(
        !generated.contains("additional_derives_ignore"),
        "{generated}"
    );
}

/// A flags struct of a sub-module is named from its module, and must be visible to the Orb module;
/// one no function uses stays ordinary Rust, without the functions the bindings call.
#[test]
fn a_flags_struct_of_a_sub_module_is_named_from_its_module() {
    let module = expanded(
        "example:office@1.0.0",
        "mod office {
            pub mod rights {
                #[flags]
                pub struct Access { pub read: bool }
                #[flags]
                pub struct Unused { pub seen: bool }
                #[service]
                pub struct Permits;
                impl Permits { pub fn check(access: Access) -> bool { access.read } }
            }
        }",
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("\"example:office/permits@1.0.0/access\" : super :: super :: super :: super :: rights :: Access"),
        "{generated}"
    );
    assert!(
        generated.contains("pub struct Unused { pub seen : bool }"),
        "{generated}"
    );
    assert!(!generated.contains("for Unused"), "{generated}");
    let error = refused(
        "example:office@1.0.0",
        "mod office {
            pub mod rights {
                #[flags]
                struct Access { pub read: bool }
                #[service]
                pub struct Permits;
                impl Permits { pub fn check() -> bool { true } }
            }
        }",
    );
    assert!(
        error.contains("`Access` is a flags of a sub-module: make it visible to the Orb module"),
        "{error}"
    );
    // An impl of the struct written in another module names it from there: a trait implemented by
    // hand is not derived again, and a function of a name the bindings call is refused.
    let office = |extra: &str| {
        format!(
            "mod office {{
                pub mod rights {{
                    #[flags]
                    pub struct Access {{ pub read: bool }}
                    #[service]
                    pub struct Permits;
                    impl Permits {{ pub fn check(access: Access) -> bool {{ access.read }} }}
                }}
                {extra}
            }}"
        )
    };
    let module = expanded(
        "example:office@1.0.0",
        &office("impl Clone for rights::Access { fn clone(&self) -> Self { *self } }"),
    );
    assert!(
        tokens(&module).contains(
            "# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access"
        ),
        "{}",
        tokens(&module)
    );
    let error = refused(
        "example:office@1.0.0",
        &office("impl rights::Access { pub fn empty() -> Self { Self { read: false } } }"),
    );
    assert!(
        error.contains("`Access::empty` takes a name the macro gives a #[flags] struct"),
        "{error}"
    );
    // A name a `use` brings into the module is the struct too, followed through the `use`, with
    // or without another type of the Orb of that name.
    let imported = |extra: &str| office(&format!("use rights::Access; {extra}"));
    let module = expanded(
        "example:office@1.0.0",
        &imported("impl Clone for Access { fn clone(&self) -> Self { *self } }"),
    );
    assert!(
        tokens(&module).contains(
            "# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access"
        ),
        "{}",
        tokens(&module)
    );
    let error = refused(
        "example:office@1.0.0",
        &imported("impl Access { pub fn empty() -> Self { Self { read: false } } }"),
    );
    assert!(
        error.contains("`Access::empty` takes a name the macro gives a #[flags] struct"),
        "{error}"
    );
    let module = expanded(
        "example:office@1.0.0",
        &imported(
            "pub mod other { pub struct Access { pub level: u32 } }
            impl Clone for Access { fn clone(&self) -> Self { *self } }",
        ),
    );
    assert!(
        tokens(&module).contains(
            "# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access { pub read : bool }"
        ),
        "{}",
        tokens(&module)
    );
}

/// The bindings name a flags struct through every module between it and the Orb module: `pub` and
/// `pub(crate)` make the struct or a module visible to the Orb module at any depth, `pub(super)`
/// one level down only, and a module the Orb module cannot see is refused, though a `pub use`
/// re-exports the struct past it.
#[test]
fn a_flags_struct_is_named_through_every_module_above_it() {
    let nested = |inner: &str, access: &str| {
        format!(
            "mod office {{
                pub mod a {{
                    {inner} mod b {{
                        #[flags]
                        {access} struct Access {{ pub read: bool }}
                        #[service]
                        pub struct Permits;
                        impl Permits {{ pub fn check(access: Access) -> bool {{ access.read }} }}
                    }}
                }}
            }}"
        )
    };
    for (inner, access) in [("pub", "pub"), ("pub(super)", "pub(crate)")] {
        let generated = tokens(&expanded("example:office@1.0.0", &nested(inner, access)));
        assert!(
            generated.contains("\"example:office/permits@1.0.0/access\" : super :: super :: super :: super :: a :: b :: Access"),
            "{generated}"
        );
    }
    let error = refused("example:office@1.0.0", &nested("pub", "pub(super)"));
    assert!(
        error.contains("`Access` is a flags of a sub-module: make it visible to the Orb module, `pub` or `pub(crate)`, whose bindings name it"),
        "{error}"
    );
    let reexported = |access: &str| {
        format!(
            "mod office {{
                mod model {{
                    {access} mod access {{
                        #[flags]
                        pub struct Access {{ pub read: bool }}
                    }}
                    pub use access::Access;
                }}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check(access: model::Access) -> bool {{ access.read }} }}
            }}"
        )
    };
    let error = refused("example:office@1.0.0", &reexported(""));
    assert!(
        error.contains("`Access` is a flags of a sub-module the Orb module cannot see: make the module `access` visible to the Orb module, `pub` or `pub(super)`, whose bindings name it"),
        "{error}"
    );
    let generated = tokens(&expanded("example:office@1.0.0", &reexported("pub(super)")));
    assert!(
        generated.contains("\"example:office/permits@1.0.0/access\" : super :: super :: super :: super :: model :: access :: Access"),
        "{generated}"
    );
}

/// What else the Orb module names in a sub-module, a state and its fields, a root resource and its
/// marked functions, and a WIT resource of a service of the Orb module, is visible to it through
/// every module between them, as a flags struct is.
#[test]
fn what_the_orb_module_names_is_visible_through_every_module_above_it() {
    let module = |inner: &str, item: &str| {
        format!(
            "mod ledger {{
                pub mod a {{
                    {inner} mod b {{
                        {item}
                    }}
                }}
                #[service]
                pub struct Passing;
                impl Passing {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    for (item, hidden, refused_item) in [
        (
            "#[state] VIS struct Balance { VIS total: i64 }",
            Some(
                "`Balance` is a state of a sub-module the Orb module cannot see: make the module `b` visible to the Orb module, `pub` or `pub(super)`, which builds and reads it",
            ),
            "`Balance` is a state of a sub-module: make the struct and its fields visible to the Orb module, `pub` or `pub(crate)`, which builds and reads it",
        ),
        (
            "#[resource] VIS struct Counts { pub seen: u32 }
            impl Counts { #[open] pub fn open() -> Self { Counts { seen: 0 } } }",
            Some(
                "`Counts` is a resource of a sub-module the Orb module cannot see: make the module `b` visible to the Orb module, `pub` or `pub(super)`, which opens and closes it",
            ),
            "`Counts` is a resource of a sub-module: make it visible to the Orb module, `pub` or `pub(crate)`, which opens and closes it",
        ),
        (
            "#[resource] pub struct Counts { pub seen: u32 }
            impl Counts { #[open] VIS fn open() -> Self { Counts { seen: 0 } } }",
            None,
            "#[open] on a function of a sub-module: make it visible to the Orb module, `pub` or `pub(crate)`, which calls it",
        ),
        (
            "#[wit_resource] VIS struct Pen { pub ink: u32 }
            impl Pen { pub fn ink(&self) -> u32 { self.ink } }",
            Some(
                "`Pen` is a resource of a sub-module the Orb module cannot see: make the module `b` visible to the Orb module, `pub` or `pub(super)`, whose service `Passing` hands it out",
            ),
            "`Pen` is a resource of a sub-module: make it visible to the Orb module, `pub` or `pub(crate)`, whose services hand it out",
        ),
    ] {
        let item = |vis: &str| item.replace("VIS", vis);
        expanded("example:ledger@1.0.0", &module("pub", &item("pub")));
        expanded(
            "example:ledger@1.0.0",
            &module("pub(super)", &item("pub(crate)")),
        );
        let error = refused("example:ledger@1.0.0", &module("pub", &item("pub(super)")));
        assert!(error.contains(refused_item), "{error}");
        if let Some(hidden) = hidden {
            let error = refused("example:ledger@1.0.0", &module("", &item("pub")));
            assert!(error.contains(hidden), "{error}");
        }
    }
}

/// The bindings name a WIT resource from its service's module, so a module between the resource
/// and the Orb module may stay hidden from the Orb module when the service sits inside it; a module
/// the service's module cannot see is refused, naming the visibility it needs there.
#[test]
fn a_wit_resource_is_named_from_its_service_module() {
    const PEN: &str = "#[wit_resource(service = Walker)]
        pub struct Pen { pub ink: u32 }
        impl Pen { pub fn ink(&self) -> u32 { self.ink } }";
    let beside = expanded(
        "example:walk@1.0.0",
        &format!(
            "mod walk {{
                pub mod a {{
                    mod b {{
                        {PEN}
                        #[service]
                        pub struct Walker;
                        impl Walker {{ pub fn lend() -> Pen {{ todo!() }} }}
                    }}
                }}
            }}"
        ),
    );
    let generated = tokens(&beside);
    assert!(
        generated.contains("type Pen = super :: super :: a :: b :: Pen ;"),
        "{generated}"
    );
    let below = expanded(
        "example:walk@1.0.0",
        &format!(
            "mod walk {{
                pub mod a {{
                    mod b {{
                        pub mod c {{
                            {PEN}
                        }}
                        #[service]
                        pub struct Walker;
                        impl Walker {{ pub fn lend() -> c::Pen {{ todo!() }} }}
                    }}
                }}
            }}"
        ),
    );
    let generated = tokens(&below);
    assert!(
        generated.contains("type Pen = super :: super :: a :: b :: c :: Pen ;"),
        "{generated}"
    );
    let above = |c: &str| {
        format!(
            "mod walk {{
                pub mod a {{
                    pub mod b {{
                        {c} mod c {{
                            {PEN}
                        }}
                    }}
                    #[service]
                    pub struct Walker;
                    impl Walker {{ pub fn lend() -> b::c::Pen {{ todo!() }} }}
                }}
            }}"
        )
    };
    let error = refused("example:walk@1.0.0", &above(""));
    assert!(
        error.contains("`Pen` is a resource of a sub-module the module `a` cannot see: make the module `c` visible to the module `a`, `pub` or `pub(super)`, whose service `Walker` hands it out"),
        "{error}"
    );
    let generated = tokens(&expanded("example:walk@1.0.0", &above("pub(super)")));
    assert!(
        generated.contains("type Pen = super :: a :: b :: c :: Pen ;"),
        "{generated}"
    );
}

/// What WIT or the bindings rule out of a flags struct is refused on the item: a field that is no
/// `bool`, no field or more than the component model's 32, a generic parameter, two fields of one
/// WIT name, an option on the attribute, a function of a name the bindings call or the author's own
/// `BitOr`, and a derived hook's input reaching the flags.
#[test]
fn what_a_flags_struct_cannot_be_is_refused() {
    let module = |access: &str, extra: &str| {
        format!(
            "mod office {{
                {access}
                {extra}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn invert(access: Access) -> Access {{ access }} }}
            }}"
        )
    };
    let many = (0..33)
        .map(|index| format!("pub f{index}: bool"))
        .collect::<Vec<_>>()
        .join(", ");
    for (access, extra, message) in [
        (
            "#[flags] pub struct Access { pub read: bool, pub level: u8 }".to_owned(),
            "",
            "the field `Access::level` is no `bool`: each field of a flags is a flag, a `bool`",
        ),
        (
            "#[flags] pub struct Access {}".to_owned(),
            "",
            "`Access` has no field: a WIT flags holds at least one flag",
        ),
        (
            format!("#[flags] pub struct Access {{ {many} }}"),
            "",
            "`Access` has 33 fields: a WIT flags holds at most 32 flags, as the component model rules",
        ),
        (
            "#[flags] pub struct Access<T> { pub read: bool, pub marker: core::marker::PhantomData<T> }".to_owned(),
            "",
            "`Access` is a flags: it takes no generic parameter",
        ),
        (
            "#[flags] pub struct Access { pub can_read: bool, pub canRead: bool }".to_owned(),
            "",
            "the fields `Access::can_read` and `Access::canRead` are both the flag `can-read`",
        ),
        (
            "#[flags(bits = 8)] pub struct Access { pub read: bool }".to_owned(),
            "",
            "#[flags] takes no options",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl Access { pub fn bits(&self) -> u8 { 0 } }",
            "`Access::bits` takes a name the macro gives a #[flags] struct, whose bindings call `empty`, `bits`, `from_bits_retain` on it",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl core::ops::BitOr for Access { type Output = Self; fn bitor(self, other: Self) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl core::ops::BitOr<Self> for Access { type Output = Self; fn bitor(self, other: Self) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl core::ops::BitOr<Access> for Access { type Output = Self; fn bitor(self, other: Self) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl<T> core::ops::BitOr<T> for Access { type Output = Self; fn bitor(self, other: T) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "impl<T: Into<Access>> core::ops::BitOr<T> for Access { type Output = Self; fn bitor(self, other: T) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "#[flags] pub struct Access { pub read: bool }".to_owned(),
            "#[hook(composition = series, input = Admission)]
            pub struct Guard;
            pub struct Admission { pub access: Access }",
            "`Access` is a flags that a derived hook's input reaches",
        ),
    ] {
        let error = refused("example:office@1.0.0", &module(&access, extra));
        assert!(error.contains(message), "{access} {extra}: {error}");
    }
    // A `BitOr` with another right-hand type does not overlap the macro's `BitOr<Self>`.
    let generated = tokens(&expanded(
        "example:office@1.0.0",
        &module(
            "#[flags] pub struct Access { pub read: bool }",
            "impl core::ops::BitOr<u32> for Access { type Output = Self; fn bitor(self, other: u32) -> Self { self } }",
        ),
    ));
    assert!(
        generated.contains("impl :: core :: ops :: BitOr for Access"),
        "{generated}"
    );
}

/// A tuple or unit struct of another module with a flags struct's name is another type: the impls
/// written for it in its module are its own, not the flags struct's, which the macro completes and
/// checks as if they were not there. An impl in a third module that names the flags struct through
/// a `use` is the flags struct's, whatever namesake another module declares.
#[test]
fn a_namesake_of_a_flags_struct_in_another_module_keeps_its_impls() {
    let module = |legacy: &str| {
        format!(
            "mod office {{
                pub mod rights {{
                    #[flags]
                    pub struct Access {{ pub read: bool, pub write: bool }}
                    #[service]
                    pub struct Guard;
                    impl Guard {{ pub fn check(access: Access) -> bool {{ access.read }} }}
                }}
                pub mod legacy {{
                    pub struct Access(pub u32);
                    {legacy}
                }}
            }}"
        )
    };
    // A `Clone` of the namesake leaves the flags struct's `Clone` to the macro.
    let generated = tokens(&expanded(
        "example:office@1.0.0",
        &module("impl Clone for Access { fn clone(&self) -> Self { Access(self.0) } }"),
    ));
    assert!(
        generated.contains(
            "# [derive (:: core :: clone :: Clone , :: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access { pub read : bool , pub write : bool }"
        ),
        "{generated}"
    );
    // A `BitOr` or a `bits` of the namesake is not the flags struct's.
    for legacy in [
        "impl core::ops::BitOr for Access { type Output = Self; fn bitor(self, other: Self) -> Self { Access(self.0 | other.0) } }",
        "impl Access { pub fn bits(&self) -> u32 { self.0 } }",
    ] {
        expanded("example:office@1.0.0", &module(legacy));
    }
    // An impl reached through a `use` in a third module is the flags struct's, whatever namesake
    // another module declares: the `use` is followed before any fallback on the name.
    let module = |ops: &str| {
        format!(
            "mod office {{
                pub mod rights {{
                    #[flags]
                    pub struct Access {{ pub read: bool, pub write: bool }}
                    #[service]
                    pub struct Guard;
                    impl Guard {{ pub fn check(access: Access) -> bool {{ access.read }} }}
                }}
                pub mod legacy {{ pub type Access = u32; }}
                pub mod ops {{
                    use super::rights::Access;
                    {ops}
                }}
            }}"
        )
    };
    let generated = tokens(&expanded(
        "example:office@1.0.0",
        &module("impl Clone for Access { fn clone(&self) -> Self { *self } }"),
    ));
    assert!(
        generated.contains(
            "# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access { pub read : bool , pub write : bool }"
        ),
        "{generated}"
    );
    for (ops, message) in [
        (
            "impl core::ops::BitOr for Access { type Output = Self; fn bitor(self, other: Self) -> Self { self } }",
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            "impl Access { pub fn bits(&self) -> u32 { 0 } }",
            "`Access::bits` takes a name the macro gives a #[flags] struct",
        ),
    ] {
        let error = refused("example:office@1.0.0", &module(ops));
        assert!(error.contains(message), "{ops}: {error}");
    }
}

/// An impl names the type through a `use` the macro follows to a `crate::` path, binding the name
/// or a module of its path, through a module a glob import brings in, `pinned::Balance` after
/// `use super::*;`, or through an alias of the type's own name, `pub type Access =
/// super::rights::Access;`: the macro cannot place the `crate::` path such a `use` leads to, so the
/// name counts by its uniqueness among the types of the Orb's modules that may enter the WIT, as a
/// name a glob import the macro cannot follow brings does, whatever type of another shape a module
/// declares under the name, and such an alias stands for the type it names, as a `use` does. A
/// glob import from a module of the Orb is followed to the type that module declares, whatever
/// namesake a third module declares. A `crate::` path written on the impl itself leaves the Orb
/// module, as before: it may name a namesake outside the Orb, whose impl is not the type's.
#[test]
fn an_impl_names_the_type_through_crate_a_glob_or_an_alias() {
    let office = |ops: &str, legacy: &str| {
        format!(
            "mod office {{
                pub mod rights {{
                    #[flags]
                    pub struct Access {{ pub read: bool, pub write: bool }}
                    #[service]
                    pub struct Guard;
                    impl Guard {{ pub fn check(access: Access) -> bool {{ access.read }} }}
                }}
                pub mod ops {{
                    {ops}
                }}
                {legacy}
            }}"
        )
    };
    let clone = "impl Clone for Access { fn clone(&self) -> Self { *self } }";
    let bitor = "impl core::ops::BitOr for Access { type Output = Self; fn bitor(self, other: Self) -> Self { self } }";
    let bits = "impl Access { pub fn bits(&self) -> u32 { 0 } }";
    let without_clone = "# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access { pub read : bool , pub write : bool }";
    for (ops, legacy) in [
        (format!("use crate::office::rights::Access; {clone}"), ""),
        (
            "use super::*; impl Clone for rights::Access { fn clone(&self) -> Self { *self } }"
                .to_owned(),
            "",
        ),
        (
            format!("pub type Access = super::rights::Access; {clone}"),
            "",
        ),
        (
            format!("use super::rights::*; {clone}"),
            "pub mod legacy { pub struct Access(pub u32); }",
        ),
        (
            "use crate::office::rights; impl Clone for rights::Access { fn clone(&self) -> Self { *self } }"
                .to_owned(),
            "",
        ),
        (
            format!("use crate::office::rights::*; {clone}"),
            "pub mod legacy { pub struct Access(pub u32); }",
        ),
    ] {
        let generated = tokens(&expanded("example:office@1.0.0", &office(&ops, legacy)));
        assert!(generated.contains(without_clone), "{ops}\n{generated}");
    }
    let generated = tokens(&expanded(
        "example:office@1.0.0",
        &office(
            "impl Clone for crate::elsewhere::Access { fn clone(&self) -> Self { todo!() } }",
            "",
        ),
    ));
    assert!(
        generated.contains(
            "# [derive (:: core :: clone :: Clone , :: core :: marker :: Copy , :: core :: fmt :: Debug)] pub struct Access { pub read : bool , pub write : bool }"
        ),
        "{generated}"
    );
    for (ops, message) in [
        (
            format!("use crate::office::rights::Access; {bitor}"),
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            format!("use crate::office::rights::Access; {bits}"),
            "`Access::bits` takes a name the macro gives a #[flags] struct",
        ),
        (
            format!("pub type Access = super::rights::Access; {bitor}"),
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
        (
            format!(
                "use crate::office::rights; {}",
                bitor.replace("for Access", "for rights::Access")
            ),
            "`Access` is a #[flags] struct, for which the macro implements `BitOr`",
        ),
    ] {
        let error = refused("example:office@1.0.0", &office(&ops, ""));
        assert!(error.contains(message), "{ops}: {error}");
    }
    // A state's `Default` written so is the author's.
    let ledger = |items: &str| {
        format!(
            "mod ledger {{
                #[state]
                pub struct Balance {{ pub total: i64 }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
                {items}
            }}"
        )
    };
    let default = "impl Default for Balance { fn default() -> Self { Balance { total: 7 } } }";
    let written = "impl :: core :: default :: Default for";
    for items in [
        format!("pub mod defaults {{ use crate::ledger::Balance; {default} }}"),
        format!("pub mod api {{ pub type Balance = super::Balance; {default} }}"),
    ] {
        let generated = tokens(&expanded("example:ledger@1.0.0", &ledger(&items)));
        assert!(!generated.contains(written), "{items}\n{generated}");
    }
    let generated = tokens(&expanded(
        "example:ledger@1.0.0",
        &ledger("impl Default for crate::elsewhere::Balance { fn default() -> Self { todo!() } }"),
    ));
    assert!(generated.contains(written), "{generated}");
    for uses in ["use super::*;", "use crate::ledger::pinned;"] {
        let generated = tokens(&expanded(
            "example:ledger@1.0.0",
            &format!(
                "mod ledger {{
                    pub mod pinned {{
                        #[state]
                        pub struct Balance {{ pub total: i64 }}
                        #[service]
                        pub struct Pinned;
                        impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
                    }}
                    pub mod defaults {{
                        {uses}
                        impl Default for pinned::Balance {{ fn default() -> Self {{ pinned::Balance {{ total: 7 }} }} }}
                    }}
                }}"
            ),
        ));
        assert!(!generated.contains(written), "{uses}\n{generated}");
    }
    // A type holding a resource handle has the `Clone` a type holding it derives when the macro
    // sees it implemented, through such a `use` too.
    expanded(
        "example:walk@1.0.0",
        "mod walk {
            #[wit_resource]
            pub struct Pen { pub ink: u32 }
            impl Pen { pub fn ink(&self) -> u32 { self.ink } }
            pub struct Lease { pub pen: Pen }
            #[derive(Clone)]
            pub struct Desk { pub lease: Lease }
            pub mod hands {
                use crate::walk::Lease;
                impl Clone for Lease { fn clone(&self) -> Self { todo!() } }
            }
            #[service]
            pub struct Walker;
            impl Walker { pub fn lend() -> Desk { todo!() } }
        }",
    );
}

/// The macro reads a type that enters the WIT before the compiler evaluates `#[cfg(..)]` and
/// `#[cfg_attr(..)]`: a field, a case or a case's payload under `#[cfg(..)]`, which the WIT would
/// hold in every configuration, is refused, on a flags as on a record, an enum or a variant, and
/// so is a `#[cfg_attr(..)]` wrapping a derive, a helper or a `#[cfg(..)]`, on a flags too, whose
/// derives the macro completes. Kept out of the WIT, the same type is the author's, as written.
#[test]
fn what_the_macro_cannot_evaluate_is_refused() {
    let module = |types: &str, returned: &str| {
        format!(
            "mod office {{
                {types}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    for (types, returned, message) in [
        (
            "#[flags] pub struct Access { pub read: bool, #[cfg(feature = \"sharing\")] pub share: bool }",
            "Access",
            "`#[cfg(..)]` on the field `Access::share`: `Access` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the field in every configuration; remove the attribute, or keep `Access` out of the WIT",
        ),
        (
            "#[flags] #[cfg_attr(test, derive(Debug))] pub struct Access { pub read: bool }",
            "Access",
            "`Access` enters the WIT as a flags, which the macro completes before the compiler evaluates `#[cfg_attr(..)]`: write the attribute without it, or keep `Access` out of the WIT",
        ),
        (
            "#[flags] pub struct Access { pub read: bool, #[cfg_attr(test, cfg(feature = \"sharing\"))] pub share: bool }",
            "Access",
            "`Access` enters the WIT as a flags, which the macro completes before the compiler evaluates `#[cfg_attr(..)]`",
        ),
        (
            "pub struct Grant { pub who: String, #[cfg(feature = \"audit\")] pub reason: String }",
            "Grant",
            "`#[cfg(..)]` on the field `Grant::reason`: `Grant` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the field in every configuration",
        ),
        (
            "pub enum Level { Read, #[cfg(feature = \"admin\")] Admin }",
            "Level",
            "`#[cfg(..)]` on the case `Level::Admin`: `Level` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the case in every configuration",
        ),
        (
            "pub enum Holder { Person(#[cfg(feature = \"named\")] String), Nobody }",
            "Holder",
            "`#[cfg(..)]` on the payload of `Holder::Person`: `Holder` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the payload in every configuration",
        ),
        (
            "pub struct Grant { #[cfg_attr(feature = \"audit\", allow(unused), cfg_attr(test, serde(skip)))] pub who: String }",
            "Grant",
            "`Grant` enters the WIT and the generated type replaces it, which takes the author's derives and the attributes they read as written, never under `#[cfg_attr(..)]`",
        ),
    ] {
        let error = refused("example:office@1.0.0", &module(types, returned));
        assert!(error.contains(message), "{types}: {error}");
        expanded("example:office@1.0.0", &module(types, "u32"));
    }
}

/// The macro writes the WIT and the bindings' `with` before the compiler evaluates `#[cfg(..)]`,
/// and a flags struct stays the author's: under `#[cfg(..)]`, the WIT would hold the flags and the
/// bindings name the struct where the compiler drops it, so a `#[flags]` struct under `#[cfg(..)]`
/// that enters the WIT is refused. The same struct without the attribute enters the WIT, and kept
/// out of the WIT, the struct is the author's, as written.
#[test]
fn a_flags_struct_under_cfg_is_refused() {
    let module = |cfg: &str, returned: &str| {
        format!(
            "mod office {{
                {cfg}
                #[flags]
                pub struct Access {{ pub read: bool, pub share: bool }}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let cfg = "#[cfg(feature = \"sharing\")]";
    let error = refused("example:office@1.0.0", &module(cfg, "Access"));
    assert!(
        error.contains("`#[cfg(..)]` on the flags `Access`: `Access` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the flags and the bindings would name the struct in every configuration; remove the attribute, or keep `Access` out of the WIT"),
        "{error}"
    );
    let entered = tokens(&expanded("example:office@1.0.0", &module("", "Access")));
    assert!(
        entered.contains(
            "\"example:office/permits@1.0.0/access\" : super :: super :: super :: super :: Access"
        ),
        "{entered}"
    );
    let kept = tokens(&expanded("example:office@1.0.0", &module(cfg, "u32")));
    assert!(
        kept.contains(
            "# [cfg (feature = \"sharing\")] pub struct Access { pub read : bool , pub share : bool }"
        ),
        "{kept}"
    );
}

/// A `#[cfg_attr(..)]` wrapping only Rust's own attributes that the generated type never takes,
/// such as `allow`, `doc` or `deprecated`, changes nothing the macro writes: it may sit on a type
/// that enters the WIT, its fields and its cases, and a flags struct keeps it as written.
#[test]
fn a_cfg_attr_wrapping_only_what_the_generated_type_never_takes_expands() {
    let module = expanded(
        "example:office@1.0.0",
        "mod office {
            #[flags]
            #[cfg_attr(docsrs, doc = \"With every feature.\")]
            pub struct Access { #[cfg_attr(test, allow(dead_code))] pub read: bool }
            #[cfg_attr(test, deprecated, cfg_attr(docsrs, doc(hidden)))]
            pub struct Grant { #[cfg_attr(test, allow(dead_code))] pub who: String, pub access: Access }
            pub enum Level { #[cfg_attr(test, allow(unused))] Read, Write }
            #[service]
            pub struct Permits;
            impl Permits { pub fn issue(level: Level) -> Grant { todo!() } }
        }",
    );
    let text = resolved(&module);
    for expected in [
        "  flags access {\n    read,\n  }\n",
        "  record grant {\n    who: string,\n    access: access,\n  }\n",
        "  enum level {\n    read,\n    write,\n  }\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    assert!(
        generated.contains(
            "pub struct Access { # [cfg_attr (test , allow (dead_code))] pub read : bool }"
        ),
        "{generated}"
    );
}

/// The macro writes the WIT, and the generated type replacing a record, an enum or a variant that
/// enters it, before the compiler evaluates `#[cfg(..)]`: the type would stay in the WIT and in the
/// generated code in every configuration, the attribute doing nothing, so a `#[cfg(..)]` on a type
/// that enters the WIT is refused. The same type without the attribute enters the WIT, and kept out
/// of the WIT, the type is the author's, as written, `#[cfg(..)]` included.
#[test]
fn a_type_entering_the_wit_under_cfg_is_refused() {
    let module = |types: &str, returned: &str| {
        format!(
            "mod office {{
                {types}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    for (kind, name, item, entered, kept) in [
        (
            "record",
            "Grant",
            "pub struct Grant { pub who: String }",
            "  record grant {\n    who: string,\n  }\n",
            "# [cfg (feature = \"audit\")] pub struct Grant { pub who : String }",
        ),
        (
            "enum",
            "Level",
            "pub enum Level { Read, Write }",
            "  enum level {\n    read,\n    write,\n  }\n",
            "# [cfg (feature = \"audit\")] pub enum Level { Read , Write }",
        ),
        (
            "variant",
            "Holder",
            "pub enum Holder { Person(String), Nobody }",
            "  variant holder {\n    person(string),\n    nobody,\n  }\n",
            "# [cfg (feature = \"audit\")] pub enum Holder { Person (String) , Nobody }",
        ),
    ] {
        let conditional = format!("#[cfg(feature = \"audit\")] {item}");
        let error = refused("example:office@1.0.0", &module(&conditional, name));
        let message = format!(
            "`#[cfg(..)]` on the {kind} `{name}`: `{name}` enters the WIT, which the macro writes before the compiler evaluates the attribute, so the WIT would hold the {kind} and the generated type would replace `{name}` in every configuration; remove the attribute, or keep `{name}` out of the WIT"
        );
        assert!(error.contains(&message), "{item}: {error}");
        let text = resolved(&expanded("example:office@1.0.0", &module(item, name)));
        assert!(text.contains(entered), "{entered}\n{text}");
        let generated = tokens(&expanded(
            "example:office@1.0.0",
            &module(&conditional, "u32"),
        ));
        assert!(generated.contains(kept), "{kept}\n{generated}");
    }
}

/// A `#[cfg(..)]` as an author writes it on an item, plainly or under `#[cfg_attr(..)]`, which
/// keeps the item in some configurations only, with how the errors name it.
const CONDITIONS: [(&str, &str); 2] = [
    ("#[cfg(feature = \"audit\")]", "`#[cfg(..)]`"),
    (
        "#[cfg_attr(test, cfg(feature = \"audit\"))]",
        "`#[cfg_attr(..)]` wrapping `#[cfg(..)]`",
    ),
];

/// The conditions an item takes refused, by whether the macro evaluates a plain `#[cfg(..)]` on it
/// itself (`cfg`): on a service, its `impl` blocks and their functions, a hook and a sub-module
/// holding one of these it does, so only the one under `#[cfg_attr(..)]` is refused there, and
/// otherwise both are.
fn refused_conditions(evaluated: bool) -> impl Iterator<Item = (&'static str, &'static str)> {
    CONDITIONS.into_iter().skip(usize::from(evaluated))
}

/// What the error asks for: the plain form where the macro evaluates it, else no attribute.
fn instead(evaluated: bool) -> &'static str {
    if evaluated {
        "write the condition as a plain `#[cfg(..)]`, which the macro evaluates"
    } else {
        "remove the attribute"
    }
}

/// The macro writes the code serving a struct it marks before the compiler evaluates
/// `#[cfg(..)]`, and that code names the struct in every configuration: the kernel's callbacks
/// build and read a state and open and close a resource of the root, a bridge calls the functions
/// of a service and of a WIT resource, and a declared hook's export and `run` serve its struct.
/// Where the attribute is off, the compiler would find the struct gone, so a `#[cfg(..)]` on a
/// marked struct is refused, before or after the marker, written plainly or under
/// `#[cfg_attr(..)]`, and the same module without it expands. On a service and on a hook the
/// macro evaluates a plain `#[cfg(..)]` itself (`cfg`), so only one under `#[cfg_attr(..)]` is
/// refused there, pointing to the plain form.
#[test]
fn a_marked_struct_under_cfg_is_refused() {
    let module = |items: &str| {
        format!(
            "mod office {{
                {items}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> u32 {{ 0 }} }}
            }}"
        )
    };
    for (items, what, name, does, evaluated) in [
        (
            "{cfg} #[state] pub struct Tally { pub count: u32 }",
            "state",
            "Tally",
            "builds and reads it",
            false,
        ),
        (
            "{cfg} #[resource] pub struct Pool { pub size: u32 }
            impl Pool { #[open] pub fn open() -> Self { Pool { size: 0 } } }",
            "resource",
            "Pool",
            "opens and closes it",
            false,
        ),
        (
            "{cfg} #[wit_resource] pub struct Badge { holder: String }
            impl Badge {
                #[constructor]
                pub fn new(holder: String) -> Self { Badge { holder } }
                pub fn holder(&self) -> String { self.holder.clone() }
            }",
            "resource",
            "Badge",
            "calls its functions",
            false,
        ),
        (
            "#[service] {cfg} pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }",
            "service",
            "Ledger",
            "calls its functions",
            true,
        ),
        (
            "{cfg} #[hook(composition = series, input = Grant)] pub struct Granted;
            pub struct Grant { pub who: String }",
            "hook",
            "Granted",
            "runs it",
            true,
        ),
    ] {
        for (condition, written) in refused_conditions(evaluated) {
            let conditional = module(&items.replace("{cfg}", condition));
            let error = refused("example:office@1.0.0", &conditional);
            let message = format!(
                "{written} on the {what} `{name}`: the macro writes the code that {does} before the compiler evaluates the attribute, so that code would name `{name}` in every configuration; {}",
                instead(evaluated)
            );
            assert!(error.contains(&message), "{condition} {items}: {error}");
        }
        expanded("example:office@1.0.0", &module(&items.replace("{cfg}", "")));
    }
}

/// The macro writes the code calling a function it reads before the compiler evaluates
/// `#[cfg(..)]`, and that code names the function in every configuration: a bridge calls the
/// `pub fn` of a service, derived or implemented, and of a WIT resource, and its constructor, the
/// kernel's callbacks a function marked `#[validate]`, `#[open]` or `#[close]`, and a hook's
/// export a contribution. Where the attribute is off, the compiler would find the function gone,
/// or a WIT resource's bridge calling itself, so a `#[cfg(..)]` on such a function, or on the
/// `impl` block holding it, is refused, written plainly or under `#[cfg_attr(..)]`, and the same
/// module without it expands. On a service's `pub fn`, the `impl` block holding it and a
/// contribution the macro evaluates a plain `#[cfg(..)]` itself (`cfg`), so only one under
/// `#[cfg_attr(..)]` is refused there, pointing to the plain form. A function the macro does not
/// read keeps its `#[cfg(..)]`.
#[test]
fn a_function_under_cfg_is_refused() {
    let office = |items: &str| {
        format!(
            "mod office {{
                {items}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> u32 {{ 0 }} }}
            }}"
        )
    };
    let keeper = consumed("cfg-keeper", LEDGER_WIT);
    let blocker = consumed("cfg-blocker", ADMISSION_WIT);
    for (package, dir, module, what, name, evaluated) in [
        (
            "example:office@1.0.0",
            None,
            office("impl Permits { {cfg} pub fn audit() -> u32 { 1 } }"),
            "function",
            "Permits::audit",
            true,
        ),
        (
            "example:office@1.0.0",
            None,
            office("{cfg} impl Permits { fn helper() {} pub fn audit() -> u32 { 1 } }"),
            "`impl` block holding",
            "Permits::audit",
            true,
        ),
        (
            "example:keeper@1.0.0",
            Some(&keeper),
            KEEPER.replace("pub fn total", "{cfg} pub fn total"),
            "function",
            "Keeper::total",
            true,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[wit_resource] pub struct Badge { holder: String }
                impl Badge {
                    #[constructor]
                    pub fn new(holder: String) -> Self { Badge { holder } }
                    {cfg} pub fn holder(&self) -> String { self.holder.clone() }
                }",
            ),
            "function",
            "Badge::holder",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[wit_resource] pub struct Badge { holder: String }
                impl Badge { #[constructor] pub fn new(holder: String) -> Self { Badge { holder } } }
                {cfg} impl Badge { pub fn holder(&self) -> String { self.holder.clone() } }",
            ),
            "`impl` block holding",
            "Badge::holder",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[wit_resource] pub struct Badge { holder: String }
                impl Badge {
                    {cfg} #[constructor]
                    pub fn new(holder: String) -> Self { Badge { holder } }
                }",
            ),
            "function",
            "Badge::new",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[state] pub struct Tally { pub count: u32 }
                impl Tally {
                    #[validate] {cfg}
                    pub fn check(&self) -> Result<(), String> { Ok(()) }
                }",
            ),
            "function",
            "Tally::check",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[resource] pub struct Pool { pub size: u32 }
                impl Pool { {cfg} #[open] pub fn open() -> Self { Pool { size: 0 } } }",
            ),
            "function",
            "Pool::open",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[resource] pub struct Pool { pub size: u32 }
                {cfg} impl Pool {
                    #[open] pub fn open() -> Self { Pool { size: 0 } }
                    #[close] pub fn close(self) {}
                }",
            ),
            "`impl` block holding",
            "Pool::open",
            false,
        ),
        (
            "example:blocker@1.0.0",
            Some(&blocker),
            "mod blocker {
                {cfg}
                #[hook(\"example:admission/guard@1.0.0\", priority = -7)]
                pub fn block(input: guard::Admission) -> guard::Admission { input }
            }"
            .to_owned(),
            "contribution",
            "block",
            true,
        ),
    ] {
        let with = || options(package, dir.map(std::path::PathBuf::as_path));
        for (condition, written) in refused_conditions(evaluated) {
            let error = refused_with(with(), &module.replace("{cfg}", condition));
            let message = format!(
                "{written} on the {what} `{name}`: the macro writes the code that calls it before the compiler evaluates the attribute, so that code would name `{name}` in every configuration; {}",
                instead(evaluated)
            );
            assert!(error.contains(&message), "{condition} {module}: {error}");
        }
        expanded_with(with(), &module.replace("{cfg}", ""));
    }
    let kept = tokens(&expanded(
        "example:office@1.0.0",
        &office(
            "impl Permits { #[cfg(test)] fn helper() -> u32 { 2 } }
            #[cfg(test)] impl Permits { fn other() {} }",
        ),
    ));
    for expected in [
        "# [cfg (test)] fn helper () -> u32 { 2 }",
        "# [cfg (test)] impl Permits { fn other () { } }",
    ] {
        assert!(kept.contains(expected), "{expected}\n{kept}");
    }
}

/// A `#[cfg(..)]` on a sub-module drops every item of the module where it is off, while the code
/// the macro writes for a struct it marks or a contribution counts on the item in every
/// configuration, as under a `#[cfg(..)]` of the item's own: the export of a service, a hook or a
/// contribution, which the bindings require, is written in the item's module and goes with it,
/// and the code written elsewhere names a state, a resource or a WIT resource through the module.
/// So a `#[cfg(..)]` on a sub-module holding such an item, in the module itself or below it, is
/// refused, written plainly, under `#[cfg_attr(..)]` or inside the module, and the same module
/// without it expands. On a sub-module holding a service or a hook the macro evaluates a plain
/// `#[cfg(..)]` itself (`cfg`), so only one under `#[cfg_attr(..)]` is refused there, pointing to
/// the plain form. A sub-module holding nothing the macro marks keeps its `#[cfg(..)]`.
#[test]
fn a_sub_module_under_cfg_holding_a_marked_item_is_refused() {
    let office = |items: &str| {
        format!(
            "mod office {{
                #[service]
                pub struct Desk;
                impl Desk {{ pub fn open() -> u32 {{ 1 }} }}
                {items}
            }}"
        )
    };
    let blocker = consumed("cfg-module-blocker", ADMISSION_WIT);
    for (package, dir, module, what, name, does, evaluated) in [
        (
            "example:office@1.0.0",
            None,
            office(
                "{cfg} pub mod audit {
                    #[service]
                    pub struct Permits;
                    impl Permits { pub fn check() -> u32 { 0 } }
                }",
            ),
            "module `audit` holding the service",
            "Permits",
            "calls its functions",
            true,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "{cfg} pub mod audit {
                    pub mod checks {
                        #[service]
                        pub struct Permits;
                        impl Permits { pub fn check() -> u32 { 0 } }
                    }
                }",
            ),
            "module `audit` holding the service",
            "Permits",
            "calls its functions",
            true,
        ),
        (
            "example:office@1.0.0",
            None,
            office("{cfg} pub mod kept { #[state] pub struct Tally { pub count: u32 } }"),
            "module `kept` holding the state",
            "Tally",
            "builds and reads it",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "{cfg} pub mod pools {
                    #[resource] pub struct Pool { pub size: u32 }
                    impl Pool { #[open] pub fn open() -> Self { Pool { size: 0 } } }
                }",
            ),
            "module `pools` holding the resource",
            "Pool",
            "opens and closes it",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "{cfg} pub mod badges {
                    #[wit_resource(service = Desk)] pub struct Badge { holder: String }
                    impl Badge {
                        #[constructor]
                        pub fn new(holder: String) -> Self { Badge { holder } }
                        pub fn holder(&self) -> String { self.holder.clone() }
                    }
                }",
            ),
            "module `badges` holding the resource",
            "Badge",
            "calls its functions",
            false,
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "{cfg} pub mod grants {
                    #[hook(composition = series, input = Grant)] pub struct Granted;
                    pub struct Grant { pub who: String }
                }",
            ),
            "module `grants` holding the hook",
            "Granted",
            "runs it",
            true,
        ),
        (
            "example:blocker@1.0.0",
            Some(&blocker),
            office(
                "{cfg} pub mod blocks {
                    #[hook(\"example:admission/guard@1.0.0\", priority = -7)]
                    pub fn block(input: super::guard::Admission) -> super::guard::Admission { input }
                }",
            ),
            "module `blocks` holding the contribution",
            "block",
            "calls it",
            true,
        ),
    ] {
        let with = || options(package, dir.map(std::path::PathBuf::as_path));
        for (condition, written) in refused_conditions(evaluated) {
            let error = refused_with(with(), &module.replace("{cfg}", condition));
            let message = format!(
                "{written} on the {what} `{name}`: the macro writes the code that {does} before the compiler evaluates the attribute, so that code would name `{name}` in every configuration; {}",
                instead(evaluated)
            );
            assert!(error.contains(&message), "{condition} {module}: {error}");
        }
        expanded_with(with(), &module.replace("{cfg}", ""));
    }
    let inner = office(
        "pub mod audit {
            #![cfg_attr(test, cfg(feature = \"audit\"))]
            #[service]
            pub struct Permits;
            impl Permits { pub fn check() -> u32 { 0 } }
        }",
    );
    let error = refused("example:office@1.0.0", &inner);
    assert!(
        error.contains("`#[cfg_attr(..)]` wrapping `#[cfg(..)]` on the module `audit` holding the service `Permits`: the macro writes the code that calls its functions before the compiler evaluates the attribute, so that code would name `Permits` in every configuration; write the condition as a plain `#[cfg(..)]`, which the macro evaluates"),
        "{error}"
    );
    let kept = tokens(&expanded(
        "example:office@1.0.0",
        &office(
            "#[cfg(feature = \"audit\")] pub mod extra {
                pub struct Note { pub text: String }
                pub fn two() -> u32 { 2 }
            }
            #[cfg(test)] mod tests { #[test] fn one() {} }",
        ),
    ));
    for expected in [
        "# [cfg (feature = \"audit\")] pub mod extra {",
        "# [cfg (test)] mod tests {",
    ] {
        assert!(kept.contains(expected), "{expected}\n{kept}");
    }
}

/// The code the macro writes calling a function it reads passes each parameter it read, before
/// the compiler evaluates `#[cfg(..)]`: where the attribute is off, the compiler drops the
/// parameter and the call passes one too many. So a `#[cfg(..)]` on a parameter, `self` included,
/// of a service's `pub fn`, derived or implemented, of a WIT resource's function or constructor,
/// of a function marked `#[validate]`, `#[open]` or `#[close]`, or of a contribution, is refused,
/// written plainly or under `#[cfg_attr(..)]`, and the same module without it expands. A function
/// the macro does not read keeps its parameter's `#[cfg(..)]`, and so does a
/// `#[cfg(test)] mod tests` holding only tests and impls.
#[test]
fn a_parameter_under_cfg_is_refused() {
    let office = |items: &str| {
        format!(
            "mod office {{
                {items}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check() -> u32 {{ 0 }} }}
            }}"
        )
    };
    let badge = |functions: &str| {
        office(&format!(
            "#[wit_resource] pub struct Badge {{ holder: String }}
            impl Badge {{ {functions} }}"
        ))
    };
    let keeper = consumed("cfg-param-keeper", LEDGER_WIT);
    let blocker = consumed("cfg-param-blocker", ADMISSION_WIT);
    let constructor = "#[constructor] pub fn new(holder: String) -> Self { Badge { holder } }";
    for (package, dir, module, what, name, param) in [
        (
            "example:office@1.0.0",
            None,
            office("impl Permits { pub fn audit(level: u32, {cfg} extra: u32) -> u32 { level } }"),
            "function",
            "Permits::audit",
            "extra",
        ),
        (
            "example:keeper@1.0.0",
            Some(&keeper),
            KEEPER.replace("add(entry", "add({cfg} entry"),
            "function",
            "Keeper::add",
            "entry",
        ),
        (
            "example:office@1.0.0",
            None,
            badge("#[constructor] pub fn new({cfg} holder: String) -> Self { Badge { holder } }"),
            "function",
            "Badge::new",
            "holder",
        ),
        (
            "example:office@1.0.0",
            None,
            badge(&format!(
                "{constructor} pub fn holder({{cfg}} &self) -> String {{ self.holder.clone() }}"
            )),
            "function",
            "Badge::holder",
            "self",
        ),
        (
            "example:office@1.0.0",
            None,
            badge(&format!(
                "{constructor} pub fn signed(&self, {{cfg}} by: String) -> String {{ by }}"
            )),
            "function",
            "Badge::signed",
            "by",
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[state] pub struct Tally { pub count: u32 }
                impl Tally {
                    #[validate]
                    pub fn check({cfg} &self) -> Result<(), String> { Ok(()) }
                }",
            ),
            "function",
            "Tally::check",
            "self",
        ),
        (
            "example:office@1.0.0",
            None,
            office(
                "#[resource] pub struct Pool { pub size: u32 }
                impl Pool {
                    #[open] pub fn open() -> Self { Pool { size: 0 } }
                    #[close] pub fn close({cfg} self) {}
                }",
            ),
            "function",
            "Pool::close",
            "self",
        ),
        (
            "example:blocker@1.0.0",
            Some(&blocker),
            "mod blocker {
                #[hook(\"example:admission/guard@1.0.0\", priority = -7)]
                pub fn block({cfg} input: guard::Admission) -> guard::Admission { input }
            }"
            .to_owned(),
            "contribution",
            "block",
            "input",
        ),
    ] {
        let with = || options(package, dir.map(std::path::PathBuf::as_path));
        for (condition, written) in CONDITIONS {
            let error = refused_with(with(), &module.replace("{cfg}", condition));
            let message = format!(
                "{written} on the parameter `{param}` of the {what} `{name}`: the macro writes the code that calls it before the compiler evaluates the attribute, so that code would pass `{param}` in every configuration; remove the attribute"
            );
            assert!(error.contains(&message), "{condition} {module}: {error}");
        }
        expanded_with(with(), &module.replace("{cfg}", ""));
    }
    let kept = tokens(&expanded(
        "example:office@1.0.0",
        &office(
            "impl Permits { fn helper(level: u32, #[cfg(test)] extra: u32) -> u32 { level } }
            #[cfg(test)]
            mod tests {
                use super::*;
                impl Permits { pub fn probe(#[cfg(test)] extra: u32) -> u32 { extra } }
                #[test]
                fn one() {}
            }",
        ),
    ));
    for expected in [
        "fn helper (level : u32 , # [cfg (test)] extra : u32)",
        "# [cfg (test)] mod tests {",
        "pub fn probe (# [cfg (test)] extra : u32)",
    ] {
        assert!(kept.contains(expected), "{expected}\n{kept}");
    }
}

/// The bindings name a flags struct that enters the WIT through every module of its path, from the
/// `with` the macro writes before the compiler evaluates `#[cfg(..)]`: where the attribute on one
/// of those modules is off, the compiler drops the struct with the module, and that path names
/// nothing. So a `#[cfg(..)]` on a sub-module holding such a struct, in the module itself or below
/// it, is refused, written plainly, under `#[cfg_attr(..)]` or inside the module, and the same
/// module without it expands. A sub-module holding a flags struct that stays out of the WIT keeps
/// its `#[cfg(..)]`, and so does a `#[cfg(test)] mod tests` holding only tests and impls.
#[test]
fn a_sub_module_under_cfg_holding_a_flags_struct_is_refused() {
    let office = |items: &str, path: &str| {
        format!(
            "mod office {{
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn check(access: {path}::Access) -> bool {{ access.read }} }}
                {items}
            }}"
        )
    };
    for (module, holding) in [
        (
            office(
                "{cfg} pub mod rights { #[flags] pub struct Access { pub read: bool } }",
                "rights",
            ),
            "rights",
        ),
        (
            office(
                "{cfg} pub mod grants {
                    pub mod rights { #[flags] pub struct Access { pub read: bool } }
                }",
                "grants::rights",
            ),
            "grants",
        ),
        (
            office(
                "pub mod grants {
                    {cfg} pub mod rights { #[flags] pub struct Access { pub read: bool } }
                }",
                "grants::rights",
            ),
            "rights",
        ),
    ] {
        for (condition, written) in CONDITIONS {
            let error = refused("example:office@1.0.0", &module.replace("{cfg}", condition));
            let message = format!(
                "{written} on the module `{holding}` holding the flags `Access`: the macro writes the code that maps the WIT flags onto it before the compiler evaluates the attribute, so that code would name `Access` in every configuration; remove the attribute"
            );
            assert!(error.contains(&message), "{condition} {module}: {error}");
        }
        expanded("example:office@1.0.0", &module.replace("{cfg}", ""));
    }
    let inner = office(
        "pub mod rights {
            #![cfg(feature = \"sharing\")]
            #[flags]
            pub struct Access { pub read: bool }
        }",
        "rights",
    );
    let error = refused("example:office@1.0.0", &inner);
    assert!(
        error.contains("`#[cfg(..)]` on the module `rights` holding the flags `Access`: the macro writes the code that maps the WIT flags onto it before the compiler evaluates the attribute, so that code would name `Access` in every configuration; remove the attribute"),
        "{error}"
    );
    let kept = tokens(&expanded(
        "example:office@1.0.0",
        &office(
            "pub mod rights { #[flags] pub struct Access { pub read: bool } }
            #[cfg(feature = \"sharing\")]
            pub mod spare { #[flags] pub struct Spare { pub seen: bool } }
            #[cfg(test)]
            mod tests {
                use super::rights::Access;
                impl Access { fn none() -> Self { Access { read: false } } }
                #[test]
                fn one() {}
            }",
            "rights",
        ),
    ));
    for expected in [
        "# [cfg (feature = \"sharing\")] pub mod spare {",
        "# [cfg (test)] mod tests {",
    ] {
        assert!(kept.contains(expected), "{expected}\n{kept}");
    }
}

const NESTED_WIT: &str = "package example:ledger@1.0.0;
interface book {
  resource entry {
    constructor(label: string);
    next: func() -> option<entry>;
    split: static func() -> tuple<entry, entry>;
  }
  type maybe = option<entry>;
  find: func(label: string) -> option<entry>;
  aliased: func(label: string) -> maybe;
  many: func() -> list<result<entry, string>>;
}
";

/// An implemented interface wraps its resources inside options, lists, results and tuples too,
/// through an alias of one: the WIT says where they are.
#[test]
fn an_implemented_interface_wraps_its_nested_resources() {
    let dir = consumed("nested", NESTED_WIT);
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[wit_resource(service = Keeper)]
            pub struct Entry { label: String }
            impl Entry {
                #[constructor]
                pub fn new(label: String) -> Self { Entry { label } }
                pub fn next(&self) -> Option<Entry> { None }
                pub fn split() -> (Entry, Entry) { todo!() }
            }
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {
                pub fn find(label: String) -> Option<Entry> { None }
                pub fn aliased(label: String) -> Option<Entry> { None }
                pub fn many() -> Vec<Result<Entry, String>> { Vec::new() }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let book = "exports :: example :: ledger :: book";
    let generated = tokens(&module);
    for expected in [
        format!(
            "fn find (label : __tembra_alloc :: string :: String) -> :: core :: option :: Option < {book} :: Entry > {{ Keeper :: find (label) . map (| item | {book} :: Entry :: new (item)) }}"
        ),
        format!(
            "fn aliased (label : __tembra_alloc :: string :: String) -> {book} :: Maybe {{ Keeper :: aliased (label) . map (| item | {book} :: Entry :: new (item)) }}"
        ),
        format!(
            "Keeper :: many () . into_iter () . map (| item | match item {{ :: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok ({book} :: Entry :: new (item)) ,"
        ),
        format!(
            "fn next (& self ,) -> :: core :: option :: Option < {book} :: Entry > {{ Entry :: next (self ,) . map (| item | {book} :: Entry :: new (item)) }}"
        ),
        format!(
            "fn split () -> ({book} :: Entry , {book} :: Entry ,) {{ {{ let (item0 , item1 ,) = Entry :: split () ; ({book} :: Entry :: new (item0) , {book} :: Entry :: new (item1) ,) }} }}"
        ),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
}

/// The constructor is the function marked `#[constructor]`, of any name; a `pub fn new` without
/// the marker is a static function of the resource, and the marked function has one shape.
#[test]
fn the_constructor_is_the_marked_function_not_the_one_named_new() {
    let renamed = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn new(n: u32) -> Session { Session { n } }",
            "fn numbered(n: u32) -> Self { Session { n } }",
        ),
    );
    assert!(
        wit(&renamed).contains("    constructor(n: u32);\n"),
        "{}",
        wit(&renamed)
    );
    assert!(
        tokens(&renamed).contains("fn new (n : u32) -> Self { Session :: numbered (n) }"),
        "{}",
        tokens(&renamed)
    );
    let unmarked = expanded("example:desk@1.0.0", &DESK.replace("#[constructor]\n", ""));
    assert!(
        !wit(&unmarked).contains("constructor("),
        "{}",
        wit(&unmarked)
    );
    assert!(
        wit(&unmarked).contains("    new: static func(n: u32) -> session;\n"),
        "{}",
        wit(&unmarked)
    );
    for wrong in [
        "pub fn new(&self) -> Session { Session { n: 0 } }",
        "pub fn new(n: u32) -> u32 { n }",
    ] {
        let error = refused(
            "example:desk@1.0.0",
            &DESK.replace("pub fn new(n: u32) -> Session { Session { n } }", wrong),
        );
        assert!(error.contains("#[constructor] marks"), "{wrong}: {error}");
    }
}

/// A state no method mentions stays the author's struct, with serde derives, and out of the WIT.
#[test]
fn a_state_enters_the_wit_only_when_a_method_mentions_it() {
    let module = expanded(
        "example:ledger@1.0.0",
        "mod ledger {
            #[state]
            pub struct Balance { pub total: i64 }
            #[service]
            pub struct Pinned;
            impl Pinned { pub fn add(amount: i64) -> i64 { Balance::update(|b| b.total += amount).total } }
        }",
    );
    assert!(!wit(&module).contains("record"), "{}", wit(&module));
    let generated = tokens(&module);
    assert!(
        generated.contains(
            "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Balance"
        ),
        "{generated}"
    );
    assert!(!generated.contains("pub use exports :: example :: ledger :: pinned :: { Balance }"));
}

/// A state that stays out of the WIT is serialized by serde, and so is every type of the module
/// its fields reach, directly or through other types, that stays out too: each gets the serde
/// derives its author did not write. A type a method mentions enters the WIT, generated with the
/// derives; a type no state reaches stays as written.
#[test]
fn the_types_a_state_out_of_the_wit_reaches_get_serde_derives() {
    let module = expanded(
        "example:ledger@1.0.0",
        "mod ledger {
            #[state]
            pub struct Balance { pub total: i64, pub phase: Phase, pub notes: Vec<Note>, pub shown: Shown }
            impl Default for Balance { fn default() -> Self { todo!() } }
            pub enum Phase { Open, Closed }
            pub struct Note { pub text: String, pub mood: Option<Mood> }
            #[derive(serde::Serialize)]
            pub enum Mood { Calm, Loud }
            pub struct Shown { pub at: u64 }
            pub struct Other { pub n: u32 }
            #[service]
            pub struct Pinned;
            impl Pinned {
                pub fn add(amount: i64) -> i64 { amount }
                pub fn last_shown() -> Shown { todo!() }
            }
        }",
    );
    let derived = "# [derive (:: serde :: Serialize , :: serde :: Deserialize)]";
    let generated = tokens(&module);
    for expected in [
        format!("{derived} pub struct Balance"),
        format!("{derived} pub enum Phase"),
        format!("{derived} pub struct Note"),
        "# [derive (serde :: Serialize)] # [derive (:: serde :: Deserialize)] pub enum Mood".into(),
        "pub use exports :: example :: ledger :: pinned :: { Shown } ;".into(),
        "pub struct Other { pub n : u32 }".into(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    assert!(
        !generated.contains(&format!("{derived} pub struct Other")),
        "{generated}"
    );
}

/// The serde derives the macro adds to a state kept out of the WIT, and to the types it reaches,
/// are those the type does not have yet: a derive the macro follows to `serde`, under another name
/// too, `Ser` after `use serde::{Serialize as Ser};`, or an impl of `serde`'s trait the Orb's
/// modules write by hand, in the type's module or another, gives the type the trait, and the macro
/// adds no second one. Another crate's trait of either name, `rkyv::Serialize`, derived or
/// implemented, is no `serde` trait, so the macro still adds `serde`'s, and neither is a trait of
/// the Orb's own named so, `pub trait Serialize` declared in the module, implemented under
/// `#[cfg(..)]` too.
#[test]
fn the_serde_derives_a_state_gets_follow_renames_and_hand_impls() {
    let module = |items: &str| {
        format!(
            "mod ledger {{
                use serde::{{Serialize as Ser, Deserialize as De}};
                #[state]
                pub struct Balance {{ pub total: i64, pub notes: Vec<Note>, pub mood: Mood }}
                pub struct Note {{ pub text: String }}
                pub enum Mood {{ Calm, Loud }}
                {items}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    let serialize = "fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { todo!() }";
    let deserialize = "fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> { todo!() }";
    for (items, expected) in [
        (
            "#[derive(Default, Ser, De)] pub struct Unused;".to_owned(),
            [
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Balance",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Note",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub enum Mood",
            ],
        ),
        (
            format!(
                "impl serde::Serialize for Balance {{ {serialize} }}
                impl<'de> De<'de> for Note {{ {deserialize} }}
                pub mod json {{
                    use serde::ser::Serialize;
                    impl Serialize for super::Mood {{ {serialize} }}
                }}"
            ),
            [
                "# [derive (:: serde :: Deserialize)] pub struct Balance",
                "# [derive (:: serde :: Serialize)] pub struct Note",
                "# [derive (:: serde :: Deserialize)] pub enum Mood",
            ],
        ),
        (
            format!(
                "impl<S> rkyv::Serialize<S> for Balance {{}}
                impl<'de> ::serde::de::Deserialize<'de> for Balance {{ {deserialize} }}"
            ),
            [
                "# [derive (:: serde :: Serialize)] pub struct Balance",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Note",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub enum Mood",
            ],
        ),
        (
            "pub trait Serialize { fn bytes(&self) -> u32; }
            impl Serialize for Balance { fn bytes(&self) -> u32 { 0 } }
            #[cfg(test)]
            impl Serialize for Note { fn bytes(&self) -> u32 { 0 } }"
                .to_owned(),
            [
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Balance",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Note",
                "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub enum Mood",
            ],
        ),
    ] {
        let generated = tokens(&expanded("example:ledger@1.0.0", &module(&items)));
        for expected in expected {
            assert!(generated.contains(expected), "{expected}\n{generated}");
        }
    }
    let renamed = module("").replace(
        "pub struct Balance",
        "#[derive(Default, Ser, De)] pub struct Balance",
    );
    let renamed = renamed.replace("pub struct Note", "#[derive(Ser)] pub struct Note");
    let renamed = renamed.replace(
        "pub enum Mood",
        "#[derive(rkyv::Serialize, De)] pub enum Mood",
    );
    let generated = tokens(&expanded("example:ledger@1.0.0", &renamed));
    for expected in [
        "# [derive (Default , Ser , De)] pub struct Balance",
        "# [derive (Ser)] # [derive (:: serde :: Deserialize)] pub struct Note",
        "# [derive (rkyv :: Serialize , De)] # [derive (:: serde :: Serialize)] pub enum Mood",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    assert!(
        !generated.contains(
            "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Balance"
        ),
        "{generated}"
    );
}

/// A type is translated to WIT only once a method, a resource or a hook reaches it: a type no
/// method mentions stays ordinary Rust whatever it holds, an array, a reference or a variant with
/// named fields, and the same type is a compile error once it enters the WIT.
#[test]
fn only_a_type_that_enters_the_wit_needs_a_wit_spelling() {
    let module = |returned: &str| {
        format!(
            "mod safe {{
                pub struct Digest {{ pub bytes: [u8; 32] }}
                pub enum Shape {{ Circle {{ radius: u32 }}, Rectangle(u32, u32) }}
                #[state]
                pub struct Sealed {{ pub digest: [u8; 4], pub shape: Shape }}
                #[resource]
                pub struct Banner {{ pub text: &'static str }}
                impl Banner {{ #[open] pub fn open() -> Self {{ Banner {{ text: \"safe\" }} }} }}
                #[service]
                pub struct Vault;
                impl Vault {{ pub fn get() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let kept = expanded("example:vault@1.0.0", &module("u32"));
    assert_eq!(
        wit(&kept),
        "package example:vault@1.0.0;\n\ninterface vault {\n  get: func() -> u32;\n}\n\nworld safe {\n  import tembra:guest/state@1.0.0;\n  export tembra:guest/state-init@1.0.0;\n  import tembra:guest/resources@1.0.0;\n  export tembra:guest/resources-init@1.0.0;\n  export vault;\n}\n"
    );
    let generated = tokens(&kept);
    let derived = "# [derive (:: serde :: Serialize , :: serde :: Deserialize)]";
    for expected in [
        "pub struct Digest { pub bytes : [u8 ; 32] }".into(),
        format!("{derived} pub struct Sealed {{ pub digest : [u8 ; 4] , pub shape : Shape }}"),
        format!("{derived} pub enum Shape {{ Circle {{ radius : u32 }} , Rectangle (u32 , u32) }}"),
        "pub struct Banner { pub text : & 'static str }".into(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    for (returned, message) in [
        ("Digest", "this type has no WIT equivalent"),
        (
            "Shape",
            "`Shape::Rectangle` carries 2 unnamed payloads: a case carries nothing, one unnamed payload or named fields",
        ),
        ("Banner", "WIT boundaries pass owned values"),
    ] {
        let error = refused("example:vault@1.0.0", &module(returned));
        assert!(error.contains(message), "{returned}: {error}");
    }
}

/// A type that enters the WIT is replaced by the generated one, which has no room for an explicit
/// discriminant and cannot carry `#[serde(..)]`: both are refused there, on the item, its fields
/// or its variants, and kept on a type that stays out of the WIT, which the macro leaves as written.
#[test]
fn what_a_replaced_type_would_drop_is_refused() {
    let module = |types: &str, returned: &str| {
        format!(
            "mod ledger {{
                {types}
                #[service]
                pub struct Desk;
                impl Desk {{ pub fn get() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let priority = "pub enum Priority { Low = 10, High = 90 }";
    let error = refused("example:ledger@1.0.0", &module(priority, "Priority"));
    assert!(
        error.contains("`Priority::Low = 10`: `Priority` enters the WIT, whose enums have no explicit discriminants"),
        "{error}"
    );
    expanded("example:ledger@1.0.0", &module(priority, "u32"));
    for (types, returned) in [
        (
            "#[serde(rename_all = \"camelCase\")] pub struct Entry { pub total_amount: i64 }",
            "Entry",
        ),
        (
            "pub struct Entry { #[serde(rename = \"t\")] pub total: i64 }",
            "Entry",
        ),
        (
            "pub enum Kind { #[serde(rename = \"p\")] Plain, Weighted(u32) }",
            "Vec<Kind>",
        ),
        (
            "#[state] #[serde(deny_unknown_fields)] pub struct Entry { pub total: i64 }",
            "Entry",
        ),
    ] {
        let error = refused("example:ledger@1.0.0", &module(types, returned));
        assert!(
            error.contains("enters the WIT and the generated type replaces it, which cannot carry `#[serde(..)]`"),
            "{types}: {error}"
        );
        expanded("example:ledger@1.0.0", &module(types, "u32"));
    }
}

const REVIEW: &str = "mod review {
    /// What a reviewer decided.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Outcome {
        Accepted,
        /// Why, and how badly.
        Refused { reason: String, severity: u32 },
        Deferred(u32),
    }
    /// A decision with its reviewer.
    pub struct Decision { pub reviewer: String, outcome: Outcome }
    /// How a reviewer voted.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Vote { Yes, No }
    #[service]
    pub struct Reviews;
    impl Reviews {
        pub fn decide(reviewer: String, vote: Vote) -> Decision { todo!() }
        pub fn outcomes(decisions: Vec<Decision>) -> Vec<Outcome> { todo!() }
        pub fn worst(outcomes: Vec<Outcome>, fallback: Option<Outcome>) -> Result<Outcome, String> { todo!() }
    }
}";

/// A case with named fields enters the WIT as a record of its own, `<enum>-<case>`, the case's
/// payload. The bindings give such a case a tuple payload, so the enum stays the author's, as
/// written, and so does every type holding it: nothing re-exports a generated type in their place,
/// and the macro converts each between the author's type and the bindings' with `From` in both
/// directions, in the type's module, a case with named fields field by field into the record. A
/// bridge spells such a type as the bindings' and converts it where it is, inside lists, options and
/// results too; any other type crosses as it is.
#[test]
fn an_enum_case_with_named_fields_gets_a_record_as_its_payload() {
    let module = expanded("example:review@1.0.0", REVIEW);
    assert_eq!(
        resolved(&module),
        "package example:review@1.0.0;

interface reviews {
  record outcome-refused {
    reason: string,
    severity: u32,
  }
  /// What a reviewer decided.
  variant outcome {
    accepted,
    refused(outcome-refused),
    deferred(u32),
  }
  /// A decision with its reviewer.
  record decision {
    reviewer: string,
    outcome: outcome,
  }
  /// How a reviewer voted.
  enum vote {
    yes,
    no,
  }
  decide: func(reviewer: string, vote: vote) -> decision;
  outcomes: func(decisions: list<decision>) -> list<outcome>;
  worst: func(outcomes: list<outcome>, fallback: option<outcome>) -> result<outcome, string>;
}

world review {
  export reviews;
}
"
    );
    let generated = tokens(&module);
    let bindings = "exports :: example :: review :: reviews";
    let into = ":: core :: convert :: Into :: into";
    for expected in [
        // The author's types stay, as written; the plain enum is replaced.
        "# [doc = \" What a reviewer decided.\"] # [derive (Debug , Clone , PartialEq)] pub enum Outcome { Accepted , # [doc = \" Why, and how badly.\"] Refused { reason : String , severity : u32 } , Deferred (u32) , }".to_owned(),
        "# [doc = \" A decision with its reviewer.\"] pub struct Decision { pub reviewer : String , outcome : Outcome }".to_owned(),
        format!("pub use {bindings} :: {{ Vote }} ;"),
        // The conversions of the enum: the named case field by field into the record.
        format!(
            "impl :: core :: convert :: From < Outcome > for {bindings} :: Outcome {{ fn from (value : Outcome) -> Self {{ match value {{ Outcome :: Accepted => Self :: Accepted , Outcome :: Refused {{ reason , severity }} => Self :: Refused ({bindings} :: OutcomeRefused {{ reason : {into} (reason) , severity : {into} (severity) }}) , Outcome :: Deferred (item) => Self :: Deferred ({into} (item)) , }} }} }}"
        ),
        format!(
            "impl :: core :: convert :: From < {bindings} :: Outcome > for Outcome {{ fn from (value : {bindings} :: Outcome) -> Self {{ match value {{ {bindings} :: Outcome :: Accepted => Self :: Accepted , {bindings} :: Outcome :: Refused (item) => Self :: Refused {{ reason : {into} (item . reason) , severity : {into} (item . severity) }} , {bindings} :: Outcome :: Deferred (item) => Self :: Deferred ({into} (item)) , }} }} }}"
        ),
        // The conversions of the record holding it, field by field.
        format!(
            "impl :: core :: convert :: From < Decision > for {bindings} :: Decision {{ fn from (value : Decision) -> Self {{ Self {{ reviewer : {into} (value . reviewer) , outcome : {into} (value . outcome) }} }} }}"
        ),
        format!(
            "impl :: core :: convert :: From < {bindings} :: Decision > for Decision {{ fn from (value : {bindings} :: Decision) -> Self {{ Self {{ reviewer : {into} (value . reviewer) , outcome : {into} (value . outcome) }} }} }}"
        ),
        // The bridges spell the bindings' types and convert where the author's types are.
        format!("fn decide (reviewer : String , vote : Vote) -> {bindings} :: Decision {{ {into} (Reviews :: decide (reviewer , vote)) }}"),
        format!(
            "fn outcomes (decisions : Vec < {bindings} :: Decision >) -> Vec < {bindings} :: Outcome > {{ Reviews :: outcomes (decisions . into_iter () . map (| item | {into} (item)) . collect ()) . into_iter () . map (| item | {into} (item)) . collect () }}"
        ),
        format!(
            "fn worst (outcomes : Vec < {bindings} :: Outcome > , fallback : Option < {bindings} :: Outcome >) -> Result < {bindings} :: Outcome , String > {{ match Reviews :: worst (outcomes . into_iter () . map (| item | {into} (item)) . collect () , fallback . map (| item | {into} (item))) {{ :: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok ({into} (item)) , :: core :: result :: Result :: Err (item) => :: core :: result :: Result :: Err ({into} (item)) , }} }}"
        ),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    for unexpected in ["{ Outcome", "Outcome }", "{ Decision", "Decision }"] {
        assert!(
            !generated.contains(&format!("pub use {bindings} :: {unexpected}")),
            "{unexpected}\n{generated}"
        );
    }
    assert!(
        !generated.contains("additional_type_attributes"),
        "{generated}"
    );
}

/// A type the macro converts goes back to its own module, where its fields are visible, with its
/// conversions, which name the bindings from there; a function of another module names it through
/// the module. Where serde serializes such a type, a state or a type a state reaches, it derives
/// serde's traits, unless the author does; one no state reaches derives nothing more. A type that
/// stays the author's keeps the names the bindings would spell otherwise: the conversions use both.
#[test]
fn a_converted_type_is_converted_in_its_own_module() {
    let module = expanded(
        "example:review@1.0.0",
        "mod review {
            pub mod verdicts {
                /// What a reviewer decided.
                pub enum Outcome { Accepted, Refused { reason: String } }
                #[derive(Debug, Clone, PartialEq)]
                pub enum Mood { Calm, Upset { since: u64, HTTPCode: u16 } }
            }
            /// The last decision, kept by the kernel.
            #[state(service = Reviews)]
            pub struct Last { pub outcome: Option<verdicts::Outcome>, pub count: u32 }
            pub struct Report { pub mood: verdicts::Mood }
            #[service]
            pub struct Reviews;
            impl Reviews {
                pub fn decide(reason: Option<String>) -> verdicts::Outcome { todo!() }
                pub fn latest() -> Last { todo!() }
                pub fn moods() -> Report { todo!() }
            }
        }",
    );
    let text = wit(&module);
    for expected in [
        "  record outcome-refused {\n    reason: string,\n  }\n  /// What a reviewer decided.\n  variant outcome {\n    accepted,\n    refused(outcome-refused),\n  }\n",
        "  record mood-upset {\n    since: u64,\n    http-code: u16,\n  }\n  variant mood {\n    calm,\n    upset(mood-upset),\n  }\n",
        "  /// The last decision, kept by the kernel.\n  record last {\n    outcome: option<outcome>,\n    count: u32,\n  }\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    let bindings = "super :: exports :: example :: review :: reviews";
    let into = ":: core :: convert :: Into :: into";
    for expected in [
        // The state and the enum it reaches derive serde's traits; the other enum does not.
        "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Last { pub outcome : Option < verdicts :: Outcome > , pub count : u32 }".to_owned(),
        "# [doc = \" What a reviewer decided.\"] # [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub enum Outcome { Accepted , Refused { reason : String } }".to_owned(),
        "# [derive (Debug , Clone , PartialEq)] pub enum Mood { Calm , Upset { since : u64 , HTTPCode : u16 } }".to_owned(),
        // The conversions sit in the sub-module and name the bindings from there, each side with
        // its own names.
        format!("impl :: core :: convert :: From < Outcome > for {bindings} :: Outcome {{ fn from (value : Outcome) -> Self {{ match value {{ Outcome :: Accepted => Self :: Accepted , Outcome :: Refused {{ reason }} => Self :: Refused ({bindings} :: OutcomeRefused {{ reason : {into} (reason) }}) , }} }} }}"),
        format!("Mood :: Upset {{ since , HTTPCode }} => Self :: Upset ({bindings} :: MoodUpset {{ since : {into} (since) , http_code : {into} (HTTPCode) }})"),
        format!("{bindings} :: Mood :: Upset (item) => Self :: Upset {{ since : {into} (item . since) , HTTPCode : {into} (item . http_code) }}"),
        format!("impl :: core :: convert :: From < Last > for exports :: example :: review :: reviews :: Last {{ fn from (value : Last) -> Self {{ Self {{ outcome : value . outcome . map (| item | {into} (item)) , count : {into} (value . count) }} }} }}"),
        // The bridges name the sub-module's types as the bindings do.
        format!("fn decide (reason : Option < String >) -> exports :: example :: review :: reviews :: Outcome {{ {into} (Reviews :: decide (reason)) }}"),
        format!("fn latest () -> exports :: example :: review :: reviews :: Last {{ {into} (Reviews :: latest ()) }}"),
        format!("fn moods () -> exports :: example :: review :: reviews :: Report {{ {into} (Reviews :: moods ()) }}"),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    assert!(
        generated.contains("pub mod verdicts { # [doc = \" What a reviewer decided.\"]"),
        "{generated}"
    );
    assert!(!generated.contains("pub use exports"), "{generated}");
}

/// What a type the macro converts cannot do is refused: a case with named fields but none of
/// them, a type or a function whose WIT name is the record's, a derived hook's input reaching the
/// enum, and a function taking or returning a resource with such a type, alone or in a container;
/// a case with two unnamed payloads stays refused.
#[test]
fn what_a_converted_type_cannot_be_is_refused() {
    let module = |types: &str, methods: &str| {
        format!(
            "mod review {{
                {types}
                #[service]
                pub struct Reviews;
                impl Reviews {{ {methods} }}
            }}"
        )
    };
    let draft = "#[wit_resource] pub struct Draft; impl Draft { #[constructor] pub fn new() -> Self { Draft } }";
    for (types, methods, message) in [
        (
            "pub enum Outcome { Accepted, Refused {} }".to_owned(),
            "pub fn decide() -> Outcome { todo!() }",
            "`Outcome::Refused {}` has no field: a case carries nothing, one unnamed payload or named fields, at least one, which enter the WIT as a record of their own",
        ),
        (
            "pub enum Outcome { Accepted, Refused(String, u32) }".to_owned(),
            "pub fn decide() -> Outcome { todo!() }",
            "`Outcome::Refused` carries 2 unnamed payloads: a case carries nothing, one unnamed payload or named fields, which enter the WIT as a record of their own",
        ),
        (
            "pub struct OutcomeRefused { pub code: u32 } pub enum Outcome { Accepted, Refused { reason: String } }".to_owned(),
            "pub fn code() -> OutcomeRefused { todo!() } pub fn decide() -> Outcome { todo!() }",
            "the type `OutcomeRefused` and the record of the case `Outcome::Refused` are both `outcome-refused` in the interface `reviews`, where types and functions share one namespace; rename one of them",
        ),
        (
            "pub enum Outcome { Accepted, Refused { reason: String } }".to_owned(),
            "pub fn decide() -> Outcome { todo!() } pub fn outcome_refused() -> u32 { 0 }",
            "the record of the case `Outcome::Refused` and `Reviews::outcome_refused` are both `outcome-refused` in the interface `reviews`",
        ),
        (
            "pub enum Outcome { Accepted, Refused { reason: String } }
            pub struct Decision { pub outcome: Outcome }
            #[hook(composition = waterfall, input = Decision)]
            pub struct Vetting;"
                .to_owned(),
            "pub fn decide() -> u32 { 0 }",
            "`Outcome` is an enum with a case with named fields that a derived hook's input reaches",
        ),
        (
            format!("pub enum Outcome {{ Accepted, Refused {{ reason: String }} }} {draft}"),
            "pub fn open(outcome: Outcome) -> Result<Draft, Outcome> { todo!() }",
            "`Reviews::open` returns `Result < Draft , Outcome >`, which holds the resource `Draft` and `Outcome`, an enum with a case with named fields: the macro converts such a type between the author's and the bindings', and converts no resource; give the case an unnamed payload, a record of the module, or keep the resource apart from the type",
        ),
        (
            format!("pub enum Outcome {{ Accepted, Opened {{ draft: Draft }} }} {draft}"),
            "pub fn open() -> Outcome { todo!() }",
            "`Reviews::open` returns `Outcome`, which holds the resource `Draft` and `Outcome`, an enum with a case with named fields",
        ),
        (
            format!(
                "pub enum Outcome {{ Accepted, Refused {{ reason: String }} }} pub struct Decision {{ pub outcome: Outcome }} {draft}"
            ),
            "pub fn open(decision: Decision, draft: (Draft, Decision)) -> u32 { 0 }",
            "`Reviews::open` takes `(Draft , Decision)`, which holds the resource `Draft` and `Decision`, which holds an enum with a case with named fields",
        ),
    ] {
        let error = refused("example:review@1.0.0", &module(&types, methods));
        assert!(error.contains(message), "{types} {methods}: {error}");
    }
}

const WALK: &str = "mod walk {
    /// A point on a grid.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, core::default::Default)]
    pub struct Point { pub x: i32, pub y: i32 }
    /// A note left on the way.
    #[derive(Clone, ::std::hash::Hash, std::fmt::Debug)]
    pub struct Note { pub text: String, pub at: Point }
    /// Where a walker faces.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub enum Heading { #[default] North, East }
    /// One move of a walk.
    #[derive(Clone, PartialEq, Eq, Hash)]
    pub enum Move { Step(Heading), Jump(Point), Rest }
    /// The notes of a walk; it derives nothing.
    pub struct Trace { pub notes: Vec<Note> }
    #[service]
    pub struct Walker;
    impl Walker {
        pub fn walk(moves: Vec<Move>) -> Point { todo!() }
        pub fn last_trace() -> Trace { todo!() }
    }
}";

/// The `additional_type_attributes` option the module hands the bindings, if any.
fn type_attributes(module: &ItemMod) -> Option<String> {
    let generated = tokens(module);
    let start = generated.find("additional_type_attributes")?;
    let end = generated[start..].find("} ,")? + start + 3;
    Some(generated[start..end].to_owned())
}

/// A type that enters the WIT is replaced by the generated type, which takes the author's derives,
/// written in any spelling, less the traits the bindings implement themselves for its kind:
/// `Debug` for every type, `Clone` and `Copy` for a record or a variant holding no string or list,
/// `Clone` alone for one holding a string, and the comparisons for an enum. The derives of the
/// standard library are named by their paths in `core`, a type deriving nothing is left out, and
/// `#[default]` goes to its case.
#[test]
fn a_type_that_enters_the_wit_keeps_its_derives() {
    let module = expanded("example:walk@1.0.0", WALK);
    assert_eq!(
        type_attributes(&module).as_deref(),
        Some(concat!(
            "additional_type_attributes : { ",
            "\"example:walk/walker@1.0.0/point\" : [# [derive (:: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash , :: core :: default :: Default)]] , ",
            "\"example:walk/walker@1.0.0/note\" : [# [derive (:: core :: hash :: Hash)]] , ",
            "\"example:walk/walker@1.0.0/heading\" : [# [derive (:: core :: hash :: Hash , :: core :: default :: Default)]] , ",
            "\"example:walk/walker@1.0.0/move\" : [# [derive (:: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash)]] } ,",
        )),
        "{}",
        tokens(&module)
    );
    assert!(
        tokens(&module).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/heading.north\" : [# [default]] } ,"
        ),
        "{}",
        tokens(&module)
    );
    // An unversioned package names its types without a version; a module deriving only what
    // the bindings implement hands them nothing.
    let unversioned = expanded("example:walk", WALK);
    assert!(
        type_attributes(&unversioned)
            .is_some_and(|option| option.contains("\"example:walk/walker/point\"")),
        "{}",
        tokens(&unversioned)
    );
    let bare = expanded(
        "example:walk@1.0.0",
        "mod walk {
            #[derive(Debug, Clone, Copy)]
            pub struct Point { pub x: i32, pub y: i32 }
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
            pub enum Heading { North, East }
            #[service]
            pub struct Walker;
            impl Walker { pub fn walk(heading: Heading) -> Point { todo!() } }
        }",
    );
    let generated = tokens(&bare);
    assert!(
        !generated.contains("additional_type_attributes"),
        "{generated}"
    );
    assert!(
        !generated.contains("additional_member_attributes"),
        "{generated}"
    );
}

/// The bindings name a derive from their own module, so the macro names it from its crate: it
/// follows the `use` and `extern crate` declarations of the type's module, renames and groups
/// included, `self::`, `super::` and the Orb's sub-modules; a first name nothing in the Orb
/// declares is a crate's, and a path from `crate::` stays; the path as written is imported again
/// in the type's module, which keeps the imports it names used. The helper attributes the derives
/// read, single names that are none of Rust's own attributes, go with them, on the type and on its
/// members; Rust's own attributes do not.
#[test]
fn a_derive_of_another_crate_is_named_from_its_crate() {
    let module = expanded(
        "example:walk@1.0.0",
        "mod walk {
            use strum::EnumIter;
            use derive_more as more;
            use ::strum::{self as s, IntoStaticStr as Static};
            extern crate serde as wire;
            use markers::Marked;
            use educe;
            use ordering::Hash;
            pub mod derives {
                pub use strum::Display;
            }
            /// Where a walker faces.
            #[derive(EnumIter, more::Constructor, s::AsRefStr, Static, self::derives::Display)]
            #[derive(crate::extra::Thing, wire::Serialize, educe::Educe, Hash)]
            #[strum(serialize_all = \"kebab-case\")]
            #[allow(dead_code)]
            pub enum Heading {
                #[strum(serialize = \"up\")]
                #[allow(dead_code)]
                North,
                East,
            }
            pub mod shapes {
                #[derive(super::Marked, PartialEq)]
                pub struct Point { #[marked(skip)] #[allow(dead_code)] pub x: i32, pub y: i32 }
            }
            #[service]
            pub struct Walker;
            impl Walker { pub fn walk(heading: Heading) -> shapes::Point { todo!() } }
        }",
    );
    assert_eq!(
        type_attributes(&module).as_deref(),
        Some(concat!(
            "additional_type_attributes : { ",
            "\"example:walk/walker@1.0.0/heading\" : [# [derive (:: strum :: EnumIter , :: derive_more :: Constructor , :: strum :: AsRefStr , :: strum :: IntoStaticStr , :: strum :: Display , crate :: extra :: Thing , :: serde :: Serialize , :: educe :: Educe , :: ordering :: Hash)] # [strum (serialize_all = \"kebab-case\")]] , ",
            "\"example:walk/walker@1.0.0/point\" : [# [derive (:: markers :: Marked , :: core :: cmp :: PartialEq)]] } ,",
        )),
        "{}",
        tokens(&module)
    );
    assert!(
        tokens(&module).contains(concat!(
            "additional_member_attributes : { ",
            "\"example:walk/walker@1.0.0/heading.north\" : [# [strum (serialize = \"up\")]] , ",
            "\"example:walk/walker@1.0.0/point.x\" : [# [marked (skip)]] } ,"
        )),
        "{}",
        tokens(&module)
    );
    // Each path is imported again, as written, in the module of its type, so the imports it goes
    // through stay used once the generated type replaces the author's.
    let generated = tokens(&module);
    for expected in [
        "# [allow (unused_imports)] use EnumIter as _ ; # [allow (unused_imports)] use more :: Constructor as _ ;",
        "{ Point } ; # [allow (unused_imports)] use super :: Marked as _ ; # [allow (unused_imports)] use PartialEq as _ ;",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// A state makes the bindings derive serde's traits on every generated type that can carry them:
/// the author's serde derives are left to them there, and handed to them in an Orb without a
/// state, where the bindings derive none. A type holding a resource handle carries them from
/// neither side.
#[test]
fn serde_derives_go_where_the_bindings_derive_none() {
    let module = |state: &str| {
        format!(
            "mod walk {{
                use serde::{{Deserialize, Serialize}};
                {state}
                #[derive(Serialize, serde::Deserialize, Hash)]
                pub struct Point {{ pub x: i32, pub y: i32 }}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> Point {{ todo!() }} }}
            }}"
        )
    };
    let stateful = expanded(
        "example:walk@1.0.0",
        &module("#[state] pub struct Count { pub steps: u32 }"),
    );
    assert_eq!(
        type_attributes(&stateful).as_deref(),
        Some(
            "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (:: core :: hash :: Hash)]] } ,"
        ),
        "{}",
        tokens(&stateful)
    );
    let stateless = expanded("example:walk@1.0.0", &module(""));
    assert_eq!(
        type_attributes(&stateless).as_deref(),
        Some(
            "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (:: serde :: Serialize , :: serde :: Deserialize , :: core :: hash :: Hash)]] } ,"
        ),
        "{}",
        tokens(&stateless)
    );
    // A record holding a resource handle has no serde implementation from the bindings, which
    // leave it out of their derives, and takes none from the author's derive either: the handle
    // type implements no serde trait, so the derive is refused.
    let error = refused(
        "example:walk@1.0.0",
        "mod walk {
            #[state]
            pub struct Count { pub steps: u32 }
            #[wit_resource]
            pub struct Pen { pub ink: u32 }
            impl Pen { pub fn ink(&self) -> u32 { self.ink } }
            #[derive(serde::Serialize)]
            pub struct Lease { pub pen: Pen }
            #[service]
            pub struct Walker;
            impl Walker { pub fn lend() -> Lease { todo!() } }
        }",
    );
    assert!(
        error.contains(
            "`serde::Serialize` on `Lease`: `Lease` enters the WIT and holds a resource handle, which implements no `Serialize`, so the generated type replacing it cannot derive `Serialize`; remove the derive, or keep `Lease` out of the WIT"
        ),
        "{error}"
    );
}

/// `Default` makes a value without a handle where the type has one: a record holding handles in
/// options and lists only, and a variant whose `#[default]` case holds none, carry it. Serde's
/// derives on a type holding a handle anywhere are refused in an Orb with a state as well, where
/// the bindings leave such a type out of their derives and the author's derive would stand alone.
#[test]
fn a_default_without_a_handle_is_carried_onto_a_type_holding_one() {
    let module = |types: &str, state: &str| {
        format!(
            "mod walk {{
                {state}
                #[wit_resource]
                pub struct Pen {{ pub ink: u32 }}
                impl Pen {{ pub fn ink(&self) -> u32 {{ self.ink }} }}
                {types}
                #[service]
                pub struct Walker;
                impl Walker {{
                    pub fn walk() -> Page {{ todo!() }}
                    pub fn lend() -> Pen {{ todo!() }}
                }}
            }}"
        )
    };
    let carried = expanded(
        "example:walk@1.0.0",
        &module(
            "#[derive(Default)]
             pub struct Page { pub next: Option<Pen>, pub all: Vec<Pen>, pub held: Held }
             #[derive(Default)]
             pub enum Held { Pen(Pen), #[default] Nothing }",
            "",
        ),
    );
    assert_eq!(
        type_attributes(&carried).as_deref(),
        Some(concat!(
            "additional_type_attributes : { ",
            "\"example:walk/walker@1.0.0/page\" : [# [derive (:: core :: default :: Default)]] , ",
            "\"example:walk/walker@1.0.0/held\" : [# [derive (:: core :: default :: Default)]] } ,",
        )),
        "{}",
        tokens(&carried)
    );
    assert!(
        tokens(&carried).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/held.nothing\" : [# [default]] } ,"
        ),
        "{}",
        tokens(&carried)
    );
    let error = refused(
        "example:walk@1.0.0",
        &module(
            "#[derive(serde::Serialize, serde::Deserialize)] pub struct Page { pub pen: Pen }",
            "#[state] pub struct Count { pub steps: u32 }",
        ),
    );
    assert!(
        error.contains(
            "`serde::Serialize` on `Page`: `Page` enters the WIT and holds a resource handle, which implements no `Serialize`"
        ),
        "{error}"
    );
}

/// A name a module imports by glob from another module of the Orb, `use super::*;` in a
/// sub-module or `use derives::*;` at the root, is followed into that module's `use` lines and
/// through its own globs, as the standard derives are left to the prelude. A glob of a module the
/// macro does not read, a crate's, may bring any name: one nothing else binds stays refused.
#[test]
fn a_glob_import_from_a_module_of_the_orb_is_followed() {
    let module = expanded(
        "example:walk@1.0.0",
        "mod walk {
            use strum::EnumIter;
            use self::derives::*;
            pub mod derives {
                pub use strum::Display;
            }
            pub mod shapes {
                use super::*;
                #[derive(EnumIter, Display, Hash)]
                pub enum Heading { North, East }
                pub mod deeper {
                    use super::*;
                    #[derive(Display)]
                    pub struct Point { pub x: i32 }
                }
            }
            #[service]
            pub struct Walker;
            impl Walker {
                pub fn walk(heading: shapes::Heading) -> shapes::deeper::Point { todo!() }
            }
        }",
    );
    assert_eq!(
        type_attributes(&module).as_deref(),
        Some(concat!(
            "additional_type_attributes : { ",
            "\"example:walk/walker@1.0.0/heading\" : [# [derive (:: strum :: EnumIter , :: strum :: Display , :: core :: hash :: Hash)]] , ",
            "\"example:walk/walker@1.0.0/point\" : [# [derive (:: strum :: Display)]] } ,",
        )),
        "{}",
        tokens(&module)
    );
    let error = refused(
        "example:walk@1.0.0",
        "mod walk {
            use strum::*;
            pub mod shapes {
                use super::*;
                #[derive(EnumIter)]
                pub enum Heading { North, East }
            }
            #[service]
            pub struct Walker;
            impl Walker { pub fn walk(heading: shapes::Heading) { todo!() } }
        }",
    );
    assert!(
        error.contains("this module imports no `EnumIter` by name, unless a glob import brings it"),
        "{error}"
    );
}

/// serde's derives are `Serialize` and `Deserialize`, which a module names by any path: where the
/// bindings derive them, the author's is left to them under a path the macro cannot follow to a
/// crate as well, `super::Serialize` from the Orb module, `crate::prelude::Serialize` or a name a
/// crate's glob may bring, as on a path from `serde`. Where the bindings derive none, such a path
/// is carried when it names the derive from the bindings' module and refused as any other when it
/// does not. Another crate's derive of either name, `rkyv::Serialize`, is that crate's, carried
/// with a state too, renamed or not, with the helper attribute it reads.
#[test]
fn a_serde_derive_the_bindings_cover_is_theirs_under_any_path() {
    let module = |imports: &str, state: &str| {
        format!(
            "mod walk {{
                {imports}
                {state}
                #[derive(Serialize, Deserialize, Hash)]
                pub struct Point {{ pub x: i32, pub y: i32 }}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> Point {{ todo!() }} }}
            }}"
        )
    };
    let state = "#[state] pub struct Count { pub steps: u32 }";
    let hash_only = "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (:: core :: hash :: Hash)]] } ,";
    for imports in [
        "use super::{Deserialize, Serialize};",
        "use crate::prelude::{Deserialize, Serialize};",
        "use serde::*;",
    ] {
        let stateful = expanded("example:walk@1.0.0", &module(imports, state));
        assert_eq!(
            type_attributes(&stateful).as_deref(),
            Some(hash_only),
            "{imports}: {}",
            tokens(&stateful)
        );
    }
    let stateless = expanded(
        "example:walk@1.0.0",
        &module("use crate::prelude::{Deserialize, Serialize};", ""),
    );
    assert_eq!(
        type_attributes(&stateless).as_deref(),
        Some(
            "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (crate :: prelude :: Serialize , crate :: prelude :: Deserialize , :: core :: hash :: Hash)]] } ,"
        ),
        "{}",
        tokens(&stateless)
    );
    let rkyv = expanded(
        "example:walk@1.0.0",
        &format!(
            "mod walk {{
                use rkyv::Serialize as Rk;
                {state}
                #[derive(rkyv::Archive, Rk, rkyv::Deserialize, Hash)]
                #[rkyv(compare(PartialEq))]
                pub struct Point {{ pub x: i32, pub y: i32 }}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> Point {{ todo!() }} }}
            }}"
        ),
    );
    assert_eq!(
        type_attributes(&rkyv).as_deref(),
        Some(
            "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (:: rkyv :: Archive , :: rkyv :: Serialize , :: rkyv :: Deserialize , :: core :: hash :: Hash)] # [rkyv (compare (PartialEq))]] } ,"
        ),
        "{}",
        tokens(&rkyv)
    );
    for (imports, message) in [
        (
            "use super::{Deserialize, Serialize};",
            "`Serialize` on `Point` leaves the Orb module, where the macro cannot follow it",
        ),
        (
            "use serde::*;",
            "this module imports no `Serialize` by name, unless a glob import brings it",
        ),
    ] {
        let error = refused("example:walk@1.0.0", &module(imports, ""));
        assert!(error.contains(message), "{imports}: {error}");
    }
    // A sub-module reaches the Orb module's `use serde::..` through `use super::*;`, which the
    // macro follows to `serde`: the derive is the bindings' with a state and carried without.
    let nested = |state: &str| {
        format!(
            "mod walk {{
                use serde::{{Deserialize, Serialize}};
                {state}
                pub mod shapes {{
                    use super::*;
                    #[derive(Serialize, Deserialize, Hash)]
                    pub struct Point {{ pub x: i32, pub y: i32 }}
                }}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> shapes::Point {{ todo!() }} }}
            }}"
        )
    };
    let stateful = expanded("example:walk@1.0.0", &nested(state));
    assert_eq!(
        type_attributes(&stateful).as_deref(),
        Some(hash_only),
        "{}",
        tokens(&stateful)
    );
    let stateless = expanded("example:walk@1.0.0", &nested(""));
    assert_eq!(
        type_attributes(&stateless).as_deref(),
        Some(
            "additional_type_attributes : { \"example:walk/walker@1.0.0/point\" : [# [derive (:: serde :: Serialize , :: serde :: Deserialize , :: core :: hash :: Hash)]] } ,"
        ),
        "{}",
        tokens(&stateless)
    );
}

/// The types a derived hook reaches are defined in its interface, which the world imports and
/// exports: the bindings generate each twice, and both take the author's derives.
#[test]
fn a_derived_hook_input_keeps_its_derives() {
    let module = expanded(
        "example:gate@1.0.0",
        &GATE.replace(
            "    /// One admission.",
            "    /// One admission.\n    #[derive(Debug, Clone, PartialEq, Eq, Hash)]",
        ),
    );
    assert_eq!(
        type_attributes(&module).as_deref(),
        Some(
            "additional_type_attributes : { \"example:gate/guard@1.0.0/admission\" : [# [derive (:: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash)]] } ,"
        ),
        "{}",
        tokens(&module)
    );
}

/// What the generated type cannot take is refused on the author's item: a derive of another crate
/// implementing a trait the bindings implement, with the helper attributes no other derive of the
/// type reads, which go with it, a derive of the standard library the type's contents rule out,
/// `Default` included on a record whose every value holds a resource handle, serde's derives on a
/// type holding one anywhere, under a path the bindings would take over in an Orb with a state
/// too, a derive the macro cannot name from the bindings' module, a helper attribute on a case's
/// payload, and a `#[cfg_attr(..)]` wrapping a derive or a helper, which would come and go. Kept
/// out of the WIT, the same type is the author's, as written.
#[test]
fn what_the_generated_type_cannot_take_is_refused() {
    let module = |types: &str, returned: &str| {
        format!(
            "mod walk {{
                use strum::*;
                #[wit_resource]
                pub struct Pen {{ pub ink: u32 }}
                impl Pen {{ pub fn ink(&self) -> u32 {{ self.ink }} }}
                {types}
                #[service]
                pub struct Walker;
                impl Walker {{
                    pub fn walk() -> {returned} {{ todo!() }}
                    pub fn lend() -> Pen {{ todo!() }}
                }}
            }}"
        )
    };
    for (types, returned, message) in [
        (
            "#[derive(derive_more::Debug)] pub struct Point { pub x: i32 }",
            "Point",
            "`derive_more::Debug` on `Point`: `Point` enters the WIT and the generated type replacing it implements `Debug` already, as the bindings implement `Debug` for every type",
        ),
        (
            "use derive_more::Debug; #[derive(Debug)] pub struct Point { pub x: i32 }",
            "Point",
            "`Debug` on `Point`: `Point` enters the WIT and the generated type replacing it implements `Debug` already",
        ),
        (
            "#[derive(educe::Clone)] pub struct Point { pub x: i32 }",
            "Point",
            "implements `Clone` already, as the bindings derive it for a record or a variant that holds no resource handle",
        ),
        (
            "#[derive(educe::Educe)] #[educe(Hash, Debug(name(Spot)))] pub struct Point { pub x: i32 }",
            "Point",
            "`educe::Educe` on `Point`: `Point` enters the WIT and the generated type replacing it implements `Debug` already, as the bindings implement `Debug` for every type; remove `Debug` from `#[educe(..)]`, or keep `Point` out of the WIT",
        ),
        (
            "#[derive(derivative::Derivative)] #[derivative(Clone(bound = \"\"))] pub struct Point { pub x: i32 }",
            "Point",
            "`derivative::Derivative` on `Point`: `Point` enters the WIT and the generated type replacing it implements `Clone` already, as the bindings derive it for a record or a variant that holds no resource handle; remove `Clone` from `#[derivative(..)]`, or keep `Point` out of the WIT",
        ),
        (
            "#[derive(more::Ord)] pub enum Heading { North, East }",
            "Heading",
            "implements `Ord` already, as the bindings derive it for every enum",
        ),
        (
            "#[derive(derive_more::Display)] pub enum Fault { Lost, Stuck }",
            "Result<u32, Fault>",
            "implements `Display` already, as the bindings implement it for the error of a function's result",
        ),
        (
            "#[derive(thiserror::Error)] pub struct Fault { pub code: u32 }",
            "Result<(), Fault>",
            "implements `Error` already, as the bindings implement it for the error of a function's result; remove the derive, or keep `Fault` out of the WIT",
        ),
        (
            "#[derive(Hash, thiserror::Error)]
             pub enum Fault { #[error(\"lost\")] Lost, #[error(\"stuck\")] Stuck }",
            "Result<u32, Fault>",
            "`thiserror::Error` on `Fault`: `Fault` enters the WIT and the generated type replacing it implements `Error` already, as the bindings implement it for the error of a function's result; remove the derive and the attributes it reads, `#[error(..)]`, or keep `Fault` out of the WIT",
        ),
        (
            "#[derive(Clone)] pub struct Lease { pub pen: Pen }",
            "Lease",
            "`Clone` on `Lease`: `Lease` enters the WIT and holds a resource handle, which implements no `Clone`",
        ),
        (
            "#[derive(PartialEq)] pub enum Held { Pen(Pen), Nothing }",
            "Held",
            "`PartialEq` on `Held`: `Held` enters the WIT and holds a resource handle, which implements no `PartialEq`",
        ),
        (
            "#[derive(Copy)] pub struct Note { pub text: String }",
            "Note",
            "`Copy` on `Note`: `Note` enters the WIT and holds a string or a list, so the generated type replacing it cannot be `Copy`",
        ),
        (
            "#[derive(Default)] pub struct Page { pub next: Pen }",
            "Page",
            "`Default` on `Page`: `Page` enters the WIT and every value of it holds a resource handle, which implements no `Default`, so the generated type replacing it cannot derive `Default`; remove the derive, or keep `Page` out of the WIT",
        ),
        (
            "#[derive(Default)] pub struct Page { pub pair: (Pen, u32) }",
            "Page",
            "`Default` on `Page`: `Page` enters the WIT and every value of it holds a resource handle",
        ),
        (
            "pub struct Slot { pub pen: Pen } #[derive(Default)] pub struct Page { pub slot: Slot }",
            "Page",
            "`Default` on `Page`: `Page` enters the WIT and every value of it holds a resource handle",
        ),
        (
            "#[derive(serde::Deserialize)] pub enum Held { Pen(Pen), Nothing }",
            "Held",
            "`serde::Deserialize` on `Held`: `Held` enters the WIT and holds a resource handle, which implements no `Deserialize`, so the generated type replacing it cannot derive `Deserialize`; remove the derive, or keep `Held` out of the WIT",
        ),
        (
            "#[derive(serde::Serialize)] pub struct Page { pub next: Option<Pen> }",
            "Page",
            "`serde::Serialize` on `Page`: `Page` enters the WIT and holds a resource handle, which implements no `Serialize`",
        ),
        (
            "#[state] pub struct Count { pub steps: u32 } #[derive(Serialize)] pub struct Lease { pub pen: Pen }",
            "Lease",
            "`Serialize` on `Lease`: `Lease` enters the WIT and holds a resource handle, which implements no `Serialize`",
        ),
        (
            "#[state] pub struct Count { pub steps: u32 } use super::Deserialize; #[derive(Deserialize)] pub struct Lease { pub pen: Pen }",
            "Lease",
            "`Deserialize` on `Lease`: `Lease` enters the WIT and holds a resource handle, which implements no `Deserialize`",
        ),
        (
            "#[derive(PartialEq, Eq)] pub struct Point { pub x: f64 }",
            "Point",
            "`Eq` on `Point`: `Point` enters the WIT and holds an `f64`, which implements no `Eq`, so the generated type replacing it cannot derive `Eq`; remove the derive, or keep `Point` out of the WIT",
        ),
        (
            "#[derive(Hash)] pub struct Point { pub x: Option<f32> }",
            "Point",
            "`Hash` on `Point`: `Point` enters the WIT and holds an `f32`, which implements no `Hash`",
        ),
        (
            "#[derive(PartialOrd, Ord)] pub enum Move { Jump((u32, f64)), Rest }",
            "Move",
            "`Ord` on `Move`: `Move` enters the WIT and holds an `f64`, which implements no `Ord`",
        ),
        (
            "#[derive(Default)] pub struct Page { pub checked: Result<u32, String> }",
            "Page",
            "`Default` on `Page`: `Page` enters the WIT and holds a `Result`, which implements no `Default`, so the generated type replacing it cannot derive `Default`; remove the derive, or keep `Page` out of the WIT",
        ),
        (
            "pub struct Slot { pub pen: Pen } #[derive(PartialEq)] pub struct Page { pub slot: Slot }",
            "Page",
            "`PartialEq` on `Page`: `Page` enters the WIT and holds a resource handle, which implements no `PartialEq`",
        ),
        (
            "pub struct Note { pub text: String } #[derive(Copy)] pub struct Page { pub note: Note }",
            "Page",
            "`Copy` on `Page`: `Page` enters the WIT and holds a string or a list, so the generated type replacing it cannot be `Copy`",
        ),
        (
            "#[derive(educe::Educe)] #[educe(Clone)] pub struct Lease { pub pen: Pen }",
            "Lease",
            "`educe::Educe` on `Lease`: `Lease` enters the WIT and holds a resource handle, which implements no `Clone`, so the generated type replacing it cannot derive `Clone`; remove `Clone` from `#[educe(..)]`, or keep `Lease` out of the WIT",
        ),
        (
            "#[derive(educe::Educe)] #[educe(Copy)] pub struct Note { pub text: String }",
            "Note",
            "`educe::Educe` on `Note`: `Note` enters the WIT and holds a string or a list, so the generated type replacing it cannot be `Copy`; remove `Copy` from `#[educe(..)]`",
        ),
        (
            "#[derive(educe::Educe)] #[educe(Eq)] pub struct Point { pub x: f64 }",
            "Point",
            "`educe::Educe` on `Point`: `Point` enters the WIT and holds an `f64`, which implements no `Eq`, so the generated type replacing it cannot derive `Eq`; remove `Eq` from `#[educe(..)]`, or keep `Point` out of the WIT",
        ),
        (
            "#[derive(educe::Educe, smart_default::SmartDefault)] #[educe(Eq)] pub struct Point { #[default(1.5)] pub x: f64 }",
            "Point",
            "`educe::Educe` on `Point`: `Point` enters the WIT and holds an `f64`, which implements no `Eq`, so the generated type replacing it cannot derive `Eq`; remove `Eq` from `#[educe(..)]`, or keep `Point` out of the WIT",
        ),
        (
            "#[derive(smart_default::SmartDefault)] pub struct Lease { pub pen: Pen }",
            "Lease",
            "`smart_default::SmartDefault` on `Lease`: `Lease` enters the WIT and every value of it holds a resource handle, which implements no `Default`, so the generated type replacing it cannot derive `Default`; remove the derive, or keep `Lease` out of the WIT",
        ),
        (
            "#[derive(EnumIter)] pub enum Heading { North, East }",
            "Heading",
            "`EnumIter` on `Heading`: `Heading` enters the WIT and the generated type replacing it is declared in the bindings' module, which `EnumIter` does not reach: this module imports no `EnumIter` by name, unless a glob import brings it; import it with `use`, or name it from its crate, such as `::name::EnumIter`",
        ),
        (
            "#[derive(super::Marked)] pub struct Point { pub x: i32 }",
            "Point",
            "`super::Marked` on `Point` leaves the Orb module, where the macro cannot follow it",
        ),
        (
            "#[derive(Default)] pub enum Move { Step(#[default] u32), Rest }",
            "Move",
            "`Move` enters the WIT and the generated type replaces it, which takes the attributes of its cases but not of their payloads: move this one to the case `Step`",
        ),
        (
            "#[cfg_attr(test, derive(Hash))] pub struct Point { pub x: i32 }",
            "Point",
            "`Point` enters the WIT and the generated type replaces it, which takes the author's derives and the attributes they read as written, never under `#[cfg_attr(..)]`",
        ),
        (
            "pub enum Heading { #[cfg_attr(test, default)] North, East }",
            "Heading",
            "never under `#[cfg_attr(..)]`",
        ),
    ] {
        let error = refused("example:walk@1.0.0", &module(types, returned));
        assert!(error.contains(message), "{types}: {error}");
        expanded("example:walk@1.0.0", &module(types, "u32"));
    }
    // A derive implementing, through its helper, only traits the bindings leave to it is carried.
    let generated = tokens(&expanded(
        "example:walk@1.0.0",
        &module(
            "#[derive(educe::Educe)] #[educe(Hash)] pub struct Point { pub x: i32 }",
            "Point",
        ),
    ));
    assert!(
        generated.contains("[# [derive (:: educe :: Educe)] # [educe (Hash)]]"),
        "{generated}"
    );
    // A member carrying the helper attribute a derive is known to read is the derive's, which may
    // give it its own value or leave it out: `#[default(..)]` of `SmartDefault` on a handle,
    // `#[educe(..)]` of `Educe` on an `f64`.
    for (types, returned, expected) in [
        (
            "#[derive(smart_default::SmartDefault)] pub struct Lease { #[default(crate::walk::pen())] pub pen: Pen }",
            "Lease",
            "\"example:walk/walker@1.0.0/lease\" : [# [derive (:: smart_default :: SmartDefault)]] } , additional_member_attributes : { \"example:walk/walker@1.0.0/lease.pen\" : [# [default (crate :: walk :: pen ())]] } ,",
        ),
        (
            "#[derive(educe::Educe)] #[educe(Hash)] pub struct Point { #[educe(Hash(ignore))] pub x: f64 }",
            "Point",
            "\"example:walk/walker@1.0.0/point\" : [# [derive (:: educe :: Educe)] # [educe (Hash)]] } , additional_member_attributes : { \"example:walk/walker@1.0.0/point.x\" : [# [educe (Hash (ignore))]] } ,",
        ),
    ] {
        let generated = tokens(&expanded("example:walk@1.0.0", &module(types, returned)));
        assert!(generated.contains(expected), "{types}: {generated}");
    }
}

const BOOK_KINDS_WIT: &str = "package example:ledger@1.0.0;
interface book {
  record entry { label: string, amount: s64 }
  enum side { credit, debit }
  flags marks { urgent, checked }
  add: func(entry: entry, side: side, marks: marks) -> u32;
}
";

/// A derive of the standard library is decided trait by trait from the type's fields, as the
/// bindings would decide it inside their module: a handle, a string or a list held through a
/// consumed type or a type of the module rules out what it rules out in a field, unless the macro
/// sees the trait implemented for that type, by the bindings, by its derives or by hand; a trait
/// such a type lacks otherwise is left to the author, who may implement it by hand where the macro
/// does not read; and a trait every field has is carried, as is `Default` over options, lists and
/// the cases of a variant, which `Default` leaves empty. A derive implementing a trait under
/// another name follows the same rule, but for a member whose helper attributes it reads, which it
/// may give its own value.
#[test]
fn a_standard_derive_is_decided_from_the_fields() {
    let dir = consumed("notes", BOOK_KINDS_WIT);
    let importing = || Options {
        imports: vec![literal("example:ledger/book@1.0.0")],
        ..options("example:notes@1.0.0", Some(&dir))
    };
    let module = |types: &str, returned: &str| {
        format!(
            "mod notes {{
                #[flags]
                pub struct Access {{ pub read: bool }}
                impl PartialEq for Access {{
                    fn eq(&self, other: &Self) -> bool {{ self.read == other.read }}
                }}
                pub enum Heading {{ North, East }}
                pub struct Spot {{ pub x: i32 }}
                {types}
                #[service]
                pub struct Shelf;
                impl Shelf {{ pub fn last() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    for (types, message) in [
        (
            "#[derive(Clone, Copy)] pub struct Note { pub entry: book::Entry }",
            "`Copy` on `Note`: `Note` enters the WIT and holds `Entry`, a record of example:ledger/book@1.0.0, which holds a string or a list, so the generated type replacing it cannot derive `Copy`; remove the derive, or keep `Note` out of the WIT",
        ),
        (
            "#[derive(Clone, Copy)] pub struct Note { pub entries: Option<(book::Entry, u32)> }",
            "`Copy` on `Note`: `Note` enters the WIT and holds `Entry`, a record of example:ledger/book@1.0.0, which holds a string or a list",
        ),
    ] {
        let error = refused_with(importing(), &module(types, "Note"));
        assert!(error.contains(message), "{types}: {error}");
        expanded_with(importing(), &module(types, "u32"));
    }
    for (types, carried) in [
        // A record of integers and strings carries its comparisons and `Hash`.
        (
            "#[derive(PartialEq, Eq, Hash)] pub struct Note { pub text: String, pub count: u32, pub tags: Vec<String> }",
            ":: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash",
        ),
        (
            "#[derive(PartialEq, PartialOrd)] pub struct Note { pub heading: Heading, pub x: f32 }",
            ":: core :: cmp :: PartialEq , :: core :: cmp :: PartialOrd",
        ),
        (
            "#[derive(PartialEq, Eq, PartialOrd, Ord)] pub struct Note { pub side: book::Side }",
            ":: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: cmp :: PartialOrd , :: core :: cmp :: Ord",
        ),
        (
            "#[derive(PartialEq, Eq, Hash)] pub struct Note { pub marks: book::Marks }",
            ":: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash",
        ),
        (
            "#[derive(Default)] pub struct Note { pub entry: Option<book::Entry>, pub spots: Vec<Spot> }",
            ":: core :: default :: Default",
        ),
        (
            "#[derive(Default)] pub enum Note { Entry(book::Entry), Spot(Spot), #[default] Blank }",
            ":: core :: default :: Default",
        ),
        (
            "#[derive(PartialEq)] pub struct Note { pub access: Access }",
            ":: core :: cmp :: PartialEq",
        ),
        (
            "#[derive(smart_default::SmartDefault)] pub struct Note { #[default(Ok(1))] pub checked: Result<u32, String> }",
            ":: smart_default :: SmartDefault",
        ),
        // A trait the author implements by hand applies to the generated type replacing the type,
        // and a state has the `Default` the macro writes for it.
        (
            "impl Default for Spot { fn default() -> Self { Spot { x: 1 } } } #[derive(Default)] pub struct Note { pub at: Spot }",
            ":: core :: default :: Default",
        ),
        (
            "#[state] pub struct Position { pub x: i32 } #[derive(Default)] pub struct Note { pub position: Position }",
            ":: core :: default :: Default",
        ),
        // A trait a type held in a field lacks may come from a hand impl the macro does not see:
        // one written through `crate::`, one on a consumed type, or one outside the Orb module.
        (
            "impl PartialEq for crate::notes::Spot { fn eq(&self, other: &Self) -> bool { self.x == other.x } } #[derive(PartialEq)] pub struct Note { pub at: Spot }",
            ":: core :: cmp :: PartialEq",
        ),
        (
            "impl PartialEq for book::Entry { fn eq(&self, other: &Self) -> bool { self.label == other.label } } #[derive(Clone, PartialEq)] pub struct Note { pub entry: book::Entry, pub qty: u32 }",
            ":: core :: cmp :: PartialEq",
        ),
        (
            "#[derive(PartialEq)] pub struct Note { pub at: Spot }",
            ":: core :: cmp :: PartialEq",
        ),
        (
            "#[derive(Hash)] pub struct Note { pub side: book::Side }",
            ":: core :: hash :: Hash",
        ),
    ] {
        let module = expanded_with(importing(), &module(types, "Note"));
        let expected = format!("\"example:notes/shelf@1.0.0/note\" : [# [derive ({carried})]");
        assert!(
            type_attributes(&module).is_some_and(|attributes| attributes.contains(&expected)),
            "{types}: {}",
            tokens(&module)
        );
    }
    // A type of the module carries what its own derives give it, and so does one holding it.
    let module = expanded_with(
        importing(),
        &module(
            "#[derive(PartialEq, Eq, Hash)] pub struct Point { pub at: Spot }",
            "Point",
        )
        .replace(
            "pub struct Spot { pub x: i32 }",
            "#[derive(PartialEq, Eq, Hash)] pub struct Spot { pub x: i32 }",
        ),
    );
    assert_eq!(
        type_attributes(&module).as_deref(),
        Some(concat!(
            "additional_type_attributes : { ",
            "\"example:notes/shelf@1.0.0/spot\" : [# [derive (:: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash)]] , ",
            "\"example:notes/shelf@1.0.0/point\" : [# [derive (:: core :: cmp :: PartialEq , :: core :: cmp :: Eq , :: core :: hash :: Hash)]] } ,",
        )),
        "{}",
        tokens(&module)
    );
}

/// The bindings implement `Debug` for every type they generate, `Clone` and `Copy` as far as its
/// contents allow, the comparisons for an enum, and `Display` and `Error` for the error of a
/// function's result, and they derive `serde`'s traits on it in an Orb with a state; the macro
/// implements `BitOr` for a flags struct. An impl of one of those traits that the Orb's modules
/// write for such a type, under `#[cfg(..)]` too, or a derive implementing `BitOr` on a flags
/// struct, would implement it twice, so it is refused, saying what implements it already; so is an
/// impl of the trait with `Self`, the type itself, placed from the impl's module, or a type
/// parameter as its type argument, `PartialEq<Self>`, which the derived `PartialEq` is. The same
/// module without it expands, and so do an impl of a trait the bindings do not implement for that
/// type, one of the trait with another right-hand type, `PartialEq<u8>` or another module's
/// namesake, which coexists with the derived one, an impl whose trait the
/// macro cannot follow to a crate, one of a trait of the Orb's own that takes a prelude's or
/// `serde`'s name, `pub trait Clone`, declared in the module or brought in by a glob import from
/// another module of the Orb, and one for a type the macro places only by its name, one a glob
/// import from outside the Orb brings into another module, which may be another type's.
#[test]
fn a_hand_impl_of_what_the_generated_code_implements_is_refused() {
    let module = |items: &str| {
        format!(
            "mod walk {{
                use std::fmt;
                #[wit_resource]
                pub struct Pen {{ pub ink: u32 }}
                impl Pen {{ pub fn ink(&self) -> u32 {{ self.ink }} }}
                pub struct Point {{ pub x: i32 }}
                pub struct Lease {{ pub pen: Pen }}
                pub enum Fault {{ Lost, Stuck }}
                pub enum Heading {{ North, East }}
                #[flags]
                pub struct Access {{ pub read: bool }}
                {items}
                #[service]
                pub struct Walker;
                impl Walker {{
                    pub fn walk(access: Access, heading: Heading) -> Result<Point, Fault> {{ todo!() }}
                    pub fn lend() -> Lease {{ todo!() }}
                }}
            }}"
        )
    };
    let fmt = "fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { todo!() }";
    let serialize = "fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { todo!() }";
    let state = "#[state] pub struct Count { pub steps: u32 }";
    for (source, message) in [
        (
            module(&format!("impl fmt::Display for Fault {{ {fmt} }}")),
            "`impl fmt::Display for Fault`: `Fault` enters the WIT and the generated type replacing it implements `Display` already, as the bindings implement it for the error of a function's result; remove the impl, or keep `Fault` out of the WIT",
        ),
        (
            module("impl std::error::Error for Fault {}"),
            "`impl std::error::Error for Fault`: `Fault` enters the WIT and the generated type replacing it implements `Error` already, as the bindings implement it for the error of a function's result",
        ),
        (
            module(&format!(
                "use core::fmt::Debug; impl Debug for Point {{ {fmt} }}"
            )),
            "`impl Debug for Point`: `Point` enters the WIT and the generated type replacing it implements `Debug` already, as the bindings implement `Debug` for every type",
        ),
        (
            module("impl Clone for Point { fn clone(&self) -> Self { todo!() } }"),
            "`impl Clone for Point`: `Point` enters the WIT and the generated type replacing it implements `Clone` already, as the bindings derive it for a record or a variant that holds no resource handle",
        ),
        (
            module(
                "impl PartialEq<Heading> for Heading { fn eq(&self, other: &Heading) -> bool { todo!() } }",
            ),
            "`impl PartialEq<Heading> for Heading`: `Heading` enters the WIT and the generated type replacing it implements `PartialEq` already, as the bindings derive it for every enum",
        ),
        (
            module(
                "pub mod ops { use super::Point; impl core::fmt::Debug for Point { fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { todo!() } } }",
            ),
            "`impl core::fmt::Debug for Point`: `Point` enters the WIT and the generated type replacing it implements `Debug` already, as the bindings implement `Debug` for every type",
        ),
        (
            module(
                "#[cfg(test)]
                mod tests {
                    use super::*;
                    impl PartialEq for Heading { fn eq(&self, other: &Self) -> bool { todo!() } }
                }",
            ),
            "`impl PartialEq for Heading`: `Heading` enters the WIT and the generated type replacing it implements `PartialEq` already, as the bindings derive it for every enum",
        ),
        (
            module(
                "impl PartialEq<Self> for Heading { fn eq(&self, other: &Self) -> bool { todo!() } }",
            ),
            "`impl PartialEq<Self> for Heading`: `Heading` enters the WIT and the generated type replacing it implements `PartialEq` already, as the bindings derive it for every enum",
        ),
        (
            module(
                "impl<T> PartialEq<T> for Heading { fn eq(&self, other: &T) -> bool { todo!() } }",
            ),
            "`impl PartialEq<T> for Heading`: `Heading` enters the WIT and the generated type replacing it implements `PartialEq` already, as the bindings derive it for every enum",
        ),
        (
            module(&format!(
                "{state} impl serde::Serialize for Point {{ {serialize} }}"
            )),
            "`impl serde::Serialize for Point`: `Point` enters the WIT and the generated type replacing it implements `Serialize` already, as the bindings derive it for a type that enters the WIT of an Orb with a state; remove the impl, or keep `Point` out of the WIT",
        ),
        (
            module("").replace("#[flags]", "#[flags] #[derive(derive_more::BitOr)]"),
            "`derive_more::BitOr` on `Access`: `Access` is a #[flags] struct, for which the macro implements `BitOr`, as its bindings combine flags with `|`; remove the derive",
        ),
    ] {
        let error = refused("example:walk@1.0.0", &source);
        assert!(error.contains(message), "{source}: {error}");
    }
    for items in [
        String::new(),
        format!("impl fmt::Display for Point {{ {fmt} }}"),
        "impl PartialEq for Point { fn eq(&self, other: &Self) -> bool { todo!() } }".to_owned(),
        "impl core::cmp::PartialEq<u8> for Heading { fn eq(&self, other: &u8) -> bool { todo!() } }".to_owned(),
        "impl PartialOrd<u8> for Heading { fn partial_cmp(&self, other: &u8) -> Option<core::cmp::Ordering> { todo!() } }
        impl PartialEq<u8> for Heading { fn eq(&self, other: &u8) -> bool { todo!() } }".to_owned(),
        "impl Clone for Lease { fn clone(&self) -> Self { todo!() } }".to_owned(),
        format!("impl serde::Serialize for Point {{ {serialize} }}"),
        "impl crate::walk::Display for Fault {} pub trait Display {}".to_owned(),
        format!("use pretty::*; impl Display for Fault {{ {fmt} }}"),
        format!("{state} impl rkyv::Serialize<u32> for Point {{}}"),
        "pub trait Clone { fn dup(&self) -> u32; } impl Clone for Point { fn dup(&self) -> u32 { 0 } }".to_owned(),
        "pub trait Ord { fn rank(&self) -> u32; } impl Ord for Heading { fn rank(&self) -> u32 { 0 } }".to_owned(),
        "pub mod traits { pub trait PartialEq { fn same(&self) -> bool; } }
        use traits::*;
        impl PartialEq for Heading { fn same(&self) -> bool { true } }".to_owned(),
        format!("{state} pub trait Serialize {{ fn bytes(&self) -> u32; }} impl Serialize for Point {{ fn bytes(&self) -> u32 {{ 0 }} }}"),
        "pub mod ops { use crate::geometry::*; impl core::fmt::Debug for Point { fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { todo!() } } }".to_owned(),
        "pub mod legacy { pub struct Heading(pub u8); }
        impl PartialEq<legacy::Heading> for Heading { fn eq(&self, other: &legacy::Heading) -> bool { todo!() } }".to_owned(),
    ] {
        expanded("example:walk@1.0.0", &module(&items));
    }
}

/// The macro reads the trait impls of the Orb's modules before the compiler evaluates
/// `#[cfg(..)]`, and leaves to them what the code it writes needs: the `Clone` of a flags struct,
/// which its `Copy` needs, and what the bindings of a record holding the struct need of it, the
/// `Default` of a state, a `serde` trait of a state kept out of the WIT or of a type its fields
/// reach, and a trait a type's derive needs of a type it holds. An impl under `#[cfg(..)]`, written
/// plainly or under `#[cfg_attr(..)]`, or in a module under one, such as `#[cfg(test)] mod tests`,
/// gives the trait in some configurations only, so it is refused, unless other impls, or a derive
/// under `#[cfg_attr(..)]`, cover the others, as a `#[cfg(not(..))]` impl beside a `#[cfg(..)]` one
/// does. The same impls without the attribute expand, and so does an impl under `#[cfg(..)]` of a
/// trait the generated code does not need: the `Debug` or the `Copy` of a flags struct no record
/// holds, or a test's own `PartialEq`.
#[test]
fn a_hand_impl_under_cfg_the_generated_code_counts_on_is_refused() {
    let office = |items: &str| {
        format!(
            "mod office {{
                #[flags]
                pub struct Access {{ pub read: bool, pub write: bool }}
                pub struct Grant {{ pub access: Access, pub level: u32 }}
                pub struct Note {{ pub text: String }}
                {items}
                #[service]
                pub struct Permits;
                impl Permits {{ pub fn invert(grant: Grant, note: Note) -> Access {{ todo!() }} }}
            }}"
        )
    };
    let walk = |items: &str| {
        format!(
            "mod walk {{
                #[wit_resource]
                pub struct Pen {{ pub ink: u32 }}
                impl Pen {{ pub fn ink(&self) -> u32 {{ self.ink }} }}
                pub struct Slot {{ pub pen: Pen }}
                #[derive(PartialEq)]
                pub struct Page {{ pub slot: Slot }}
                {items}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn turn() -> Page {{ todo!() }} }}
            }}"
        )
    };
    let ledger = |items: &str| {
        format!(
            "mod ledger {{
                #[state]
                pub struct Balance {{ pub total: i64 }}
                {items}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    let compared = |items: &str| {
        office(items).replace("pub struct Grant", "#[derive(PartialEq)] pub struct Grant")
    };
    let clone = "impl Clone for Access { fn clone(&self) -> Self { *self } }";
    let debug = "impl core::fmt::Debug for Access { fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { todo!() } }";
    let equal = "impl PartialEq for Access { fn eq(&self, other: &Self) -> bool { todo!() } }";
    let compare = "impl PartialEq for Slot { fn eq(&self, other: &Self) -> bool { todo!() } }";
    let default = "impl Default for Balance { fn default() -> Self { Balance { total: 100 } } }";
    let serialize = "impl serde::Serialize for Balance { fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { todo!() } }";
    for (build, hand, place, owner, name) in [
        (
            &office as &dyn Fn(&str) -> String,
            clone,
            "`impl Clone for Access`",
            "Access",
            "Clone",
        ),
        (
            &office,
            debug,
            "`impl core::fmt::Debug for Access`",
            "Access",
            "Debug",
        ),
        (
            &compared,
            equal,
            "`impl PartialEq for Access`",
            "Access",
            "PartialEq",
        ),
        (
            &walk,
            compare,
            "`impl PartialEq for Slot`",
            "Slot",
            "PartialEq",
        ),
        (
            &ledger,
            default,
            "`impl Default for Balance`",
            "Balance",
            "Default",
        ),
        (
            &ledger,
            serialize,
            "`impl serde::Serialize for Balance`",
            "Balance",
            "Serialize",
        ),
    ] {
        for (condition, written) in CONDITIONS {
            let error = refused(
                "example:office@1.0.0",
                &build(&format!("{condition} {hand}")),
            );
            let message = format!(
                "{written} on {place}: the macro reads the impl before the compiler evaluates the attribute and counts on it, while the code it writes needs `{owner}` to implement `{name}` in every configuration; remove the attribute, or implement `{name}` for the configurations it turns off too, under `#[cfg(not(..))]`"
            );
            assert!(error.contains(&message), "{condition} {hand}: {error}");
        }
        expanded("example:office@1.0.0", &build(hand));
        for (condition, opposite) in [
            (
                "#[cfg(feature = \"audit\")]",
                "#[cfg(not(feature = \"audit\"))]",
            ),
            (
                "#[cfg_attr(test, cfg(feature = \"audit\"))]",
                "#[cfg_attr(test, cfg(not(feature = \"audit\")))]",
            ),
            (
                "#[cfg(all(unix, feature = \"audit\"))]",
                "#[cfg(any(not(unix), not(feature = \"audit\")))]",
            ),
        ] {
            expanded(
                "example:office@1.0.0",
                &build(&format!("{condition} {hand} {opposite} {hand}")),
            );
        }
    }
    let error = refused(
        "example:office@1.0.0",
        &office(&format!(
            "#[cfg(test)] mod tests {{ use super::Access; {clone} }}"
        )),
    );
    assert!(
        error.contains("`#[cfg(..)]` on the module `tests` holding `impl Clone for Access`: the macro reads the impl before the compiler evaluates the attribute and counts on it, while the code it writes needs `Access` to implement `Clone` in every configuration"),
        "{error}"
    );
    let error = refused(
        "example:office@1.0.0",
        &ledger(&format!(
            "#[cfg(feature = \"seeded\")] {default} #[cfg(test)] {default}"
        )),
    );
    assert!(
        error.contains("`#[cfg(..)]` on `impl Default for Balance`"),
        "{error}"
    );
    // A derive under `#[cfg_attr(..)]` covers the configurations an impl leaves out.
    let paired = tokens(&expanded(
        "example:office@1.0.0",
        &ledger(&format!("#[cfg(feature = \"compact\")] {serialize}")).replace(
            "pub struct Balance",
            "#[cfg_attr(not(feature = \"compact\"), derive(serde::Serialize))] pub struct Balance",
        ),
    ));
    assert!(
        paired.contains("# [derive (:: serde :: Deserialize)] pub struct Balance"),
        "{paired}"
    );
    let not_cloned = tokens(&expanded(
        "example:office@1.0.0",
        &office(&format!("#[cfg(test)] {clone} #[cfg(not(test))] {clone}")),
    ));
    assert!(
        not_cloned.contains("# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)]"),
        "{not_cloned}"
    );
    // What the generated code does not need stays the author's, under `#[cfg(..)]`.
    expanded(
        "example:office@1.0.0",
        &office(
            "#[cfg(test)]
            mod tests {
                use super::*;
                impl PartialEq for Access { fn eq(&self, other: &Self) -> bool { todo!() } }
                impl PartialEq for Note { fn eq(&self, other: &Self) -> bool { todo!() } }
                impl Default for Note { fn default() -> Self { todo!() } }
            }",
        ),
    );
    let unheld = office(&format!(
        "#[cfg(feature = \"audit\")] {debug} #[cfg(feature = \"audit\")] impl Copy for Access {{}}"
    ))
    .replace("grant: Grant", "access: Access");
    expanded("example:office@1.0.0", &unheld);
    // A type a state's fields reach is serialized with the state, and gets the serde derives the
    // macro writes for it but where impls give the trait.
    let reached = |items: &str| {
        ledger(&format!("pub struct Note {{ pub text: String }} {items}"))
            .replace("pub total: i64", "pub total: i64, pub notes: Vec<Note>")
    };
    let note = "impl serde::Serialize for Note { fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { todo!() } }";
    let error = refused(
        "example:office@1.0.0",
        &reached(&format!("#[cfg(feature = \"compact\")] {note}")),
    );
    assert!(
        error.contains("`#[cfg(..)]` on `impl serde::Serialize for Note`: the macro reads the impl before the compiler evaluates the attribute and counts on it, while the code it writes needs `Note` to implement `Serialize` in every configuration"),
        "{error}"
    );
    let paired = tokens(&expanded(
        "example:office@1.0.0",
        &reached(&format!(
            "#[cfg(feature = \"compact\")] {note} #[cfg(not(feature = \"compact\"))] {note}"
        )),
    ));
    assert!(
        paired.contains("# [derive (:: serde :: Deserialize)] pub struct Note"),
        "{paired}"
    );
    expanded(
        "example:office@1.0.0",
        &reached(&format!("#[cfg(feature = \"compact\")] {note}")).replace(
            "pub struct Note",
            "#[cfg_attr(not(feature = \"compact\"), derive(serde::Serialize))] pub struct Note",
        ),
    );
}

/// Two modules of one name, which `#[cfg(..)]`s select, are two modules: the macro reads the
/// impls and the attributes of each, and puts a type it reads back in the type's own. A state's
/// `Default` or a flags struct's `Clone` written in a `#[cfg(x)]` module and again in a
/// `#[cfg(not(x))]` one of the same name gives the trait in every configuration, as the same
/// impls side by side do; a sub-module of the second, beside a state kept out of the WIT or a
/// type that enters it, and a type kept out of the WIT in either stay where they are written; and
/// the second, holding a sub-module that holds a flags struct, is refused at its `#[cfg(..)]`, as
/// any module holding one is.
#[test]
fn modules_of_one_name_under_cfg_are_each_read_as_their_own() {
    let ledger = |items: &str| {
        format!(
            "mod ledger {{
                #[state]
                pub struct Balance {{ pub total: i64 }}
                {items}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    let office = |items: &str| {
        format!(
            "mod office {{
                #[flags]
                pub struct Access {{ pub read: bool }}
                {items}
                #[service]
                pub struct Guard;
                impl Guard {{ pub fn check(access: Access) -> bool {{ access.read }} }}
            }}"
        )
    };
    let default = |total: u32| {
        format!(
            "impl Default for super::Balance {{ fn default() -> Self {{ super::Balance {{ total: {total} }} }} }}"
        )
    };
    let seeded = tokens(&expanded(
        "example:ledger@1.0.0",
        &ledger(&format!(
            "#[cfg(feature = \"seeded\")] mod seed {{ {} }}
             #[cfg(not(feature = \"seeded\"))] mod seed {{ {} }}",
            default(1),
            default(0)
        )),
    ));
    assert!(
        !seeded.contains("impl :: core :: default :: Default for Balance"),
        "{seeded}"
    );
    let clone = "impl Clone for super::Access { fn clone(&self) -> Self { *self } }";
    let cloned = tokens(&expanded(
        "example:office@1.0.0",
        &office(&format!(
            "#[cfg(feature = \"audit\")] mod hands {{ {clone} }}
             #[cfg(not(feature = \"audit\"))] mod hands {{ {clone} }}"
        )),
    ));
    assert!(
        cloned.contains("# [derive (:: core :: marker :: Copy , :: core :: fmt :: Debug)]"),
        "{cloned}"
    );
    let audit = "#[cfg(feature = \"audit\")] mod audit { pub struct Entry { pub at: u64 } }
        #[cfg(not(feature = \"audit\"))] mod audit { pub struct Entry { pub at: u32 } mod sink {} }";
    for module in [ledger(audit), office(audit)] {
        let kept = tokens(&expanded("example:ledger@1.0.0", &module));
        for written in [
            "# [cfg (feature = \"audit\")] mod audit { pub struct Entry { pub at : u64 } }",
            "# [cfg (not (feature = \"audit\"))] mod audit { pub struct Entry { pub at : u32 } mod sink { } }",
        ] {
            assert!(kept.contains(written), "{written}\n{kept}");
        }
    }
    let error = refused(
        "example:office@1.0.0",
        "mod office {
            #[cfg(feature = \"audit\")]
            mod rights {}
            #[cfg(not(feature = \"audit\"))]
            pub mod rights {
                pub mod grants {
                    #[flags]
                    pub struct Access { pub read: bool }
                }
            }
            #[service]
            pub struct Guard;
            impl Guard { pub fn check(access: rights::grants::Access) -> bool { access.read } }
        }",
    );
    assert!(
        error.contains("`#[cfg(..)]` on the module `rights` holding the flags `Access`: the macro writes the code that maps the WIT flags onto it before the compiler evaluates the attribute"),
        "{error}"
    );
}

/// The values written for a configuration name that takes one at a time, `target_endian =
/// "little"` and `target_endian = "big"`, may be all it takes, so the macro cannot be sure a
/// configuration sets none of them: impls split across them give the trait, as a `#[cfg(x)]` one
/// beside a `#[cfg(not(x))]` one does. One value alone leaves the others out, the features and the
/// target features a configuration sets are any of them, together or none, and an impl that needs
/// another option too leaves out the configurations without it, so those are refused.
#[test]
fn impls_split_across_the_values_of_one_name_give_the_trait() {
    let ledger = |items: &str| {
        format!(
            "mod ledger {{
                #[state]
                pub struct Balance {{ pub total: i64 }}
                {items}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    let default = "impl Default for Balance { fn default() -> Self { Balance { total: 100 } } }";
    for split in [
        "#[cfg(target_endian = \"little\")] {default} #[cfg(target_endian = \"big\")] {default}",
        "#[cfg(target_pointer_width = \"32\")] {default} #[cfg(target_pointer_width = \"64\")] {default}",
        "#[cfg(any(target_endian = \"little\", feature = \"audit\"))] {default}
         #[cfg(all(target_endian = \"big\", not(feature = \"audit\")))] {default}",
    ] {
        let expanded = tokens(&expanded(
            "example:ledger@1.0.0",
            &ledger(&split.replace("{default}", default)),
        ));
        assert!(
            !expanded.contains("impl :: core :: default :: Default for Balance"),
            "{split}: {expanded}"
        );
    }
    for split in [
        "#[cfg(target_endian = \"little\")] {default}",
        "#[cfg(all(target_pointer_width = \"32\", feature = \"audit\"))] {default}
         #[cfg(target_pointer_width = \"64\")] {default}",
        "#[cfg(feature = \"audit\")] {default} #[cfg(feature = \"seeded\")] {default}",
        "#[cfg(target_feature = \"simd128\")] {default} #[cfg(target_feature = \"atomics\")] {default}",
    ] {
        let error = refused(
            "example:ledger@1.0.0",
            &ledger(&split.replace("{default}", default)),
        );
        assert!(
            error.contains("`#[cfg(..)]` on `impl Default for Balance`: the macro reads the impl before the compiler evaluates the attribute and counts on it, while the code it writes needs `Balance` to implement `Default` in every configuration"),
            "{split}: {error}"
        );
    }
}

/// A derive under a `#[cfg_attr(..)]` inside another applies where both predicates hold, so it
/// covers what impls under `#[cfg(..)]` leave out of a state's `Default` or serde traits only
/// there: `#[cfg_attr(feature = "a", cfg_attr(feature = "b", derive(Default)))]` beside an impl
/// under `#[cfg(not(feature = "a"))]` leaves out the configurations with `a` and without `b`, and is
/// refused, while an impl under `#[cfg(not(all(feature = "a", feature = "b")))]` covers them.
#[test]
fn a_derive_under_nested_cfg_attr_covers_where_both_predicates_hold() {
    let ledger = |trait_name: &str, condition: &str, body: &str| {
        format!(
            "mod ledger {{
                #[state]
                #[cfg_attr(feature = \"a\", cfg_attr(feature = \"b\", derive({trait_name})))]
                pub struct Balance {{ pub total: i64 }}
                #[cfg({condition})]
                impl {trait_name} for Balance {{ {body} }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ amount }} }}
            }}"
        )
    };
    let default = "fn default() -> Self { Balance { total: 100 } }";
    let serialize = "fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> { todo!() }";
    for (trait_name, body) in [("Default", default), ("serde::Serialize", serialize)] {
        let error = refused(
            "example:ledger@1.0.0",
            &ledger(trait_name, "not(feature = \"a\")", body),
        );
        assert!(
            error.contains(&format!("`#[cfg(..)]` on `impl {trait_name} for Balance`: the macro reads the impl before the compiler evaluates the attribute and counts on it")),
            "{trait_name}: {error}"
        );
        expanded(
            "example:ledger@1.0.0",
            &ledger(
                trait_name,
                "not(all(feature = \"a\", feature = \"b\"))",
                body,
            ),
        );
    }
}

/// The bindings' module reads the helper attributes the generated type takes, so one it would not
/// know there is refused on the author's type: one whose arguments name what the type's module
/// declares or imports, which a path from `crate::` names from anywhere; an attribute macro the
/// module imports, which no derive reads; and one no derive the generated type takes reads, as
/// when a glob import's `Debug` is read as the standard library's, which the bindings implement.
/// Kept out of the WIT, the same type is the author's, as written.
#[test]
fn a_helper_attribute_the_bindings_module_cannot_read_is_refused() {
    let module = |types: &str, returned: &str| {
        format!(
            "mod walk {{
                {types}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let carried = expanded(
        "example:walk@1.0.0",
        &module(
            "const PAGE: u32 = 20;
             #[derive(smart_default::SmartDefault)]
             pub struct Page { #[default(crate::walk::PAGE)] pub size: u32 }",
            "Page",
        ),
    );
    assert!(
        tokens(&carried).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/page.size\" : [# [default (crate :: walk :: PAGE)]] } ,"
        ),
        "{}",
        tokens(&carried)
    );
    // A type of the module that enters the WIT is declared in the bindings' module too, under the
    // same name, where the owner's interface defines or uses it: an attribute naming it reads there.
    let carried = expanded(
        "example:walk@1.0.0",
        &module(
            "pub enum Mode { Fast, Slow }
             #[derive(smart_default::SmartDefault)]
             pub struct Page { #[default(Mode::Slow)] pub mode: Mode, pub size: u32 }",
            "Page",
        ),
    );
    assert!(
        tokens(&carried).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/page.mode\" : [# [default (Mode :: Slow)]] } ,"
        ),
        "{}",
        tokens(&carried)
    );
    // So is one another interface defines and the owner's interface `use`s for a function or a
    // type of its own, which the bindings' module declares as an alias.
    let carried = expanded(
        "example:walk@1.0.0",
        "mod walk {
            pub enum Mode { Fast, Slow }
            #[service]
            pub struct Settings;
            impl Settings { pub fn current() -> Mode { todo!() } }
            #[derive(smart_default::SmartDefault)]
            pub struct Page { #[default(Mode::Slow as u32)] pub level: u32 }
            #[service]
            pub struct Walker;
            impl Walker {
                pub fn speed(mode: Mode) -> u32 { 0 }
                pub fn walk(page: Page) -> u32 { 0 }
            }
        }",
    );
    assert!(
        tokens(&carried).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/page.level\" : [# [default (Mode :: Slow as u32)]] } ,"
        ),
        "{}",
        tokens(&carried)
    );
    // A derive's helper attribute resolves in the macro namespace, so an item of its name the
    // module imports, a module here, shadows nothing: the derive that reads it by name takes it.
    let carried = expanded(
        "example:walk@1.0.0",
        &module(
            "use core::default;
             #[derive(Default)]
             pub enum Mode { #[default] Fast, Slow }",
            "Mode",
        ),
    );
    assert!(
        tokens(&carried).contains(
            "additional_member_attributes : { \"example:walk/walker@1.0.0/mode.fast\" : [# [default]] } ,"
        ),
        "{}",
        tokens(&carried)
    );
    for (types, message) in [
        (
            "const PAGE: u32 = 20;
             #[derive(smart_default::SmartDefault)]
             pub struct Page { #[default(PAGE)] pub size: u32 }",
            "`#[default(..)]` on `Page` names `PAGE` from this module: `Page` enters the WIT and the generated type replacing it is declared in the bindings' module, which reads the attribute and does not reach `PAGE`; name it from `crate::` or from its crate, `::name::..`, or keep `Page` out of the WIT",
        ),
        (
            "use crate::limits::PAGE;
             #[derive(smart_default::SmartDefault)]
             pub struct Page { #[default(PAGE)] pub size: u32 }",
            "`#[default(..)]` on `Page` names `PAGE` from this module",
        ),
        (
            "use serde_with::skip_serializing_none;
             #[skip_serializing_none]
             #[derive(serde::Serialize)]
             pub struct Page { pub size: Option<u32> }",
            "`#[skip_serializing_none]` on `Page`: `Page` enters the WIT and the generated type replacing it takes its derives and the helper attributes they read, which come with the derives, while this module imports `skip_serializing_none`, an attribute macro, which the generated type cannot take; remove it, or keep `Page` out of the WIT",
        ),
        (
            "use derive_more::*;
             #[derive(Debug)]
             pub struct Page { #[debug(skip)] pub size: u32 }",
            "`#[debug(..)]` on `Page`: `Page` enters the WIT and the generated type replacing it takes no derive that reads this attribute: a derive of the standard library reads none but `#[default]`, for `Default`, and a derive of what the bindings implement themselves is left to them; remove the attribute, or keep `Page` out of the WIT",
        ),
    ] {
        let error = refused("example:walk@1.0.0", &module(types, "Page"));
        assert!(error.contains(message), "{types}: {error}");
        expanded("example:walk@1.0.0", &module(types, "u32"));
    }
}

/// A known derive reads some of its helper attributes as code, in the bindings' module: a string
/// it parses as code, derivative's `value` or smart_default's `_code`, or the names thiserror's
/// string captures, `{PAGE}`, and, under a glob import the macro cannot follow, `use super::*;` in
/// the Orb module, the tokens of smart_default's `#[default(..)]` and the format arguments of
/// thiserror's `#[error(..)]` and derive_more's `#[display(..)]`, which the glob may bring a name
/// to. A name there that the bindings' module does not reach is refused, with the `crate::`
/// advice; the `crate::` form expands, and so do a field of the type, a name of the prelude, a
/// primitive type and a lowercase name before `::`, which may be a crate's. Elsewhere a word may be
/// the derive's own syntax, as `ignore` in `#[from(ignore)]`, which stays as written.
#[test]
fn a_helper_attribute_naming_what_a_glob_or_a_string_brings_is_refused() {
    let module = |items: &str, returned: &str| {
        format!(
            "mod walk {{
                use super::*;
                {items}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let limit = |attribute: &str| {
        format!("#[derive(thiserror::Error)] {attribute} pub struct Limit {{ pub limit: u32 }}")
    };
    let display = |attribute: &str| {
        format!("#[derive(derive_more::Display)] {attribute} pub struct Limit {{ pub limit: u32 }}")
    };
    let page = |attribute: &str| {
        format!(
            "#[derive(smart_default::SmartDefault)] pub struct Page {{ {attribute} pub size: u32 }}"
        )
    };
    let derivative = |value: &str| {
        format!(
            "#[derive(derivative::Derivative)]
            #[derivative(Default)]
            pub struct Page {{ #[derivative(Default(value = \"{value}\"))] pub size: u32 }}"
        )
    };
    let advice = "enters the WIT and the generated type replacing it is declared in the bindings' module, which reads the attribute and does not reach";
    for (refused_items, returned, shown, name, expanding) in [
        (
            limit("#[error(\"limit {} of {}\", limit, PAGE)]"),
            "Limit",
            "#[error(..)]",
            "PAGE",
            vec![
                limit("#[error(\"limit {} of {}\", limit, crate::PAGE)]"),
                limit("#[error(\"limit {} of {:?}\", limit, None::<u32>)]"),
            ],
        ),
        (
            limit("#[error(\"limit {limit} of {PAGE}\")]"),
            "Limit",
            "#[error(..)]",
            "PAGE",
            vec![
                limit("#[error(\"limit {limit} of {}\", crate::PAGE)]"),
                limit("#[error(\"limit {limit:?} of {{PAGE}}\")]"),
            ],
        ),
        (
            format!(
                "const PAGE: u32 = 20; {}",
                limit("#[error(\"limit {limit} of {PAGE}\")]")
            ),
            "Limit",
            "#[error(..)]",
            "PAGE",
            vec![format!(
                "const PAGE: u32 = 20; {}",
                limit("#[error(\"limit {limit} of {}\", crate::walk::PAGE)]")
            )],
        ),
        (
            display("#[display(\"{} of {}\", limit, PAGE)]"),
            "Limit",
            "#[display(..)]",
            "PAGE",
            vec![
                display("#[display(\"{} of {}\", limit, crate::PAGE)]"),
                display("#[display(\"{} of {}\", limit, u32::MAX)]"),
            ],
        ),
        (
            page("#[default(PAGE)]"),
            "Page",
            "#[default(..)]",
            "PAGE",
            vec![
                page("#[default(crate::PAGE)]"),
                page("#[default(Some(20).unwrap_or_default())]"),
                page("#[default(limits::PAGE)]"),
            ],
        ),
        (
            page("#[default(Limits::PAGE)]"),
            "Page",
            "#[default(..)]",
            "Limits::PAGE",
            vec![page("#[default(crate::Limits::PAGE)]")],
        ),
        (
            derivative("PAGE"),
            "Page",
            "#[derivative(..)]",
            "PAGE",
            vec![
                derivative("crate::PAGE"),
                derivative("u32::MAX"),
                derivative("Default::default()"),
            ],
        ),
        (
            format!("const PAGE: u32 = 20; {}", derivative("PAGE")),
            "Page",
            "#[derivative(..)]",
            "PAGE",
            vec![format!(
                "const PAGE: u32 = 20; {}",
                derivative("crate::walk::PAGE")
            )],
        ),
        (
            page("#[default(_code = \"PAGE * 2\")]"),
            "Page",
            "#[default(..)]",
            "PAGE",
            vec![page("#[default(_code = \"crate::PAGE * 2\")]")],
        ),
        (
            format!(
                "const PAGE: u32 = 20; {}",
                page("#[default(_code = \"PAGE * 2\")]")
            ),
            "Page",
            "#[default(..)]",
            "PAGE",
            vec![format!(
                "const PAGE: u32 = 20; {}",
                page("#[default(_code = \"crate::walk::PAGE * 2\")]")
            )],
        ),
    ] {
        let error = refused("example:walk@1.0.0", &module(&refused_items, returned));
        let owner = returned;
        let message = format!(
            "`{shown}` on `{owner}` names `{name}` from this module: `{owner}` {advice} `{name}`; name it from `crate::` or from its crate, `::name::..`, or keep `{owner}` out of the WIT"
        );
        assert!(error.contains(&message), "{refused_items}: {error}");
        for items in expanding {
            expanded("example:walk@1.0.0", &module(&items, returned));
        }
    }
    // A word of the derive's own syntax names nothing, under the same glob.
    expanded(
        "example:walk@1.0.0",
        &module(
            "#[derive(derive_more::From)]
            pub enum Value { Int(i32), #[from(ignore)] Other(i32) }",
            "Value",
        ),
    );
}

/// Under a glob import the macro cannot follow, what a known derive reads as code names from the
/// glob only the names that start a path from its scope. A format argument's `name =`, which the
/// string captures as `{max}`, a struct literal's field, a name the code binds itself, in a
/// closure's parameters or in the pattern of a `let`, a `for` or a match arm, and an attribute or
/// a macro's arguments, which are their own syntax, are no such path, so each expands, as does
/// code declaring an item, whose names the macro does not follow; a path beside them is refused as
/// before.
#[test]
fn a_helper_argument_naming_no_path_from_its_scope_expands_under_a_glob() {
    let module = |items: &str, returned: &str| {
        format!(
            "mod walk {{
                use super::*;
                {items}
                #[service]
                pub struct Walker;
                impl Walker {{ pub fn walk() -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    let limit = |attribute: &str| {
        format!("#[derive(thiserror::Error)] {attribute} pub struct Limit {{ pub limit: u32 }}")
    };
    let page = |attribute: &str| {
        format!(
            "#[derive(smart_default::SmartDefault)] pub struct Page {{ {attribute} pub size: u32 }}"
        )
    };
    let limits = |attribute: &str| {
        format!(
            "pub struct Limits {{ pub max: u32 }}
            #[derive(smart_default::SmartDefault)]
            pub struct Page {{ {attribute} pub limits: Limits }}"
        )
    };
    let advice = "enters the WIT and the generated type replacing it is declared in the bindings' module, which reads the attribute and does not reach";
    for (refused_items, returned, shown, expanding) in [
        (
            limit("#[error(\"limit {limit} of {max}\", max = PAGE)]"),
            "Limit",
            "#[error(..)]",
            vec![
                limit("#[error(\"limit {limit} (at most {max})\", max = u32::MAX)]"),
                limit("#[error(\"limit {limit} of {max}\", max = crate::PAGE)]"),
            ],
        ),
        (
            page("#[default((|n: u32| n + PAGE)(1))]"),
            "Page",
            "#[default(..)]",
            vec![
                page("#[default((|n: u32| n + 1)(1))]"),
                page("#[default({ let n = 2; n * 2 })]"),
                page("#[default({ let mut sum = 0; for n in 1..3 { sum += n; } sum })]"),
                page("#[default(match Some(2) { Some(n) if n > 1 => n, _ => 0 })]"),
                page("#[default({ #[allow(unused_mut)] let mut n = 2; n })]"),
                page("#[default(stringify!(PAGE).len() as u32)]"),
                page("#[default({ const N: u32 = 2; N })]"),
            ],
        ),
        (
            limits("#[default(Limits { max: PAGE })]"),
            "Page",
            "#[default(..)]",
            vec![limits("#[default(Limits { max: 10 })]")],
        ),
    ] {
        let error = refused("example:walk@1.0.0", &module(&refused_items, returned));
        let owner = returned;
        let message = format!(
            "`{shown}` on `{owner}` names `PAGE` from this module: `{owner}` {advice} `PAGE`; name it from `crate::` or from its crate, `::name::..`, or keep `{owner}` out of the WIT"
        );
        assert!(error.contains(&message), "{refused_items}: {error}");
        for items in expanding {
            expanded("example:walk@1.0.0", &module(&items, returned));
        }
    }
}

/// A state starts from its `Default`: the macro implements it field by field unless the author
/// gives it, by an `impl Default` anywhere in the Orb module or by a derive, which stays on a
/// state out of the WIT and goes to the generated type replacing a state that enters it; the serde
/// derives the author already wrote are not added again. A derive implementing `Default` under
/// another name, `SmartDefault` or `Derivative` with `#[derivative(Default)]`, is the state's
/// `Default` too.
#[test]
fn the_author_may_give_a_state_its_default_and_its_derives() {
    let generated_default = "impl :: core :: default :: Default for Balance {";
    let state = |attributes: &str, extra: &str, returned: &str| {
        format!(
            "mod ledger {{
                {attributes}
                #[state]
                pub struct Balance {{ pub total: i64 }}
                {extra}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> {returned} {{ todo!() }} }}
            }}"
        )
    };
    // Kept out of the WIT, the author's derives stay and complete what the macro adds.
    let derived = tokens(&expanded(
        "example:ledger@1.0.0",
        &state(
            "#[derive(Default, serde::Serialize, serde::Deserialize)]",
            "",
            "i64",
        ),
    ));
    assert!(!derived.contains(generated_default), "{derived}");
    assert!(!derived.contains("# [derive (:: serde"), "{derived}");
    let half = tokens(&expanded(
        "example:ledger@1.0.0",
        &state("#[derive(serde::Serialize)]", "", "i64"),
    ));
    assert!(
        half.contains(
            "# [derive (serde :: Serialize)] # [derive (:: serde :: Deserialize)] pub struct Balance"
        ),
        "{half}"
    );
    assert!(half.contains(generated_default), "{half}");
    // A hand-written `impl Default` is the state's, whether the state enters the WIT or not.
    let by_hand = "impl Default for Balance { fn default() -> Self { Balance { total: 100 } } }";
    for returned in ["i64", "Balance"] {
        let module = expanded("example:ledger@1.0.0", &state("", by_hand, returned));
        assert!(!tokens(&module).contains(generated_default), "{returned}");
    }
    // A state that enters the WIT is the generated type, which takes the author's derive: the
    // derived `Default` is the state's, and the macro implements none.
    for derive in [
        "Default",
        "core::default::Default",
        "::std::default::Default",
    ] {
        let entered = tokens(&expanded(
            "example:ledger@1.0.0",
            &state(&format!("#[derive({derive})]"), "", "Balance"),
        ));
        assert!(!entered.contains(generated_default), "{entered}");
        assert!(
            entered.contains("additional_type_attributes : { \"example:ledger/pinned@1.0.0/balance\" : [# [derive (:: core :: default :: Default)]] } ,"),
            "{entered}"
        );
    }
    // So does a derive that implements `Default` under another name, in or out of the WIT.
    for (attributes, carried) in [
        (
            "#[derive(smart_default::SmartDefault)]",
            "[# [derive (:: smart_default :: SmartDefault)]]",
        ),
        (
            "#[derive(derivative::Derivative)] #[derivative(Default(bound = \"\"))]",
            "[# [derive (:: derivative :: Derivative)] # [derivative (Default (bound = \"\"))]]",
        ),
    ] {
        let kept = tokens(&expanded(
            "example:ledger@1.0.0",
            &state(attributes, "", "i64"),
        ));
        assert!(!kept.contains(generated_default), "{kept}");
        let entered = tokens(&expanded(
            "example:ledger@1.0.0",
            &state(attributes, "", "Balance"),
        ));
        assert!(!entered.contains(generated_default), "{entered}");
        assert!(
            entered.contains(&format!(
                "additional_type_attributes : {{ \"example:ledger/pinned@1.0.0/balance\" : {carried} }} ,"
            )),
            "{entered}"
        );
    }
    // Two states of one name are told apart by the path the `impl` names.
    for (place, skipped, kept) in [
        (
            "    pub mod pinned {",
            "pinned :: Balance",
            "passing :: Balance",
        ),
        (
            "    pub mod pinned {\n        impl Default for super::passing::Balance { fn default() -> Self { super::passing::Balance { total: 1 } } }",
            "passing :: Balance",
            "pinned :: Balance",
        ),
    ] {
        let source = if place.contains("super::passing") {
            LEDGER.replace("    pub mod pinned {", place)
        } else {
            LEDGER.replace(
                "    pub mod pinned {",
                "    pub mod pinned {\n        impl Default for Balance { fn default() -> Self { Balance { total: 1 } } }",
            )
        };
        let generated = tokens(&expanded("example:ledger@1.0.0", &source));
        assert!(
            !generated.contains(&format!("impl :: core :: default :: Default for {skipped}")),
            "{generated}"
        );
        assert!(
            generated.contains(&format!("impl :: core :: default :: Default for {kept}")),
            "{generated}"
        );
    }
}

/// The `Default` the macro writes for a state follows each field's `#[cfg(..)]`, as a derived one
/// does: the field's initializer carries the attribute, and none of the field's others, so the
/// value holds the fields the struct holds in every configuration.
#[test]
fn the_default_the_macro_writes_follows_the_fields_cfg() {
    let generated = tokens(&expanded(
        "example:ledger@1.0.0",
        "mod ledger {
            #[state]
            pub struct Balance {
                /// What the balance holds.
                pub total: i64,
                #[cfg(feature = \"audit\")]
                #[serde(default)]
                pub audited: u32,
            }
            #[service]
            pub struct Pinned;
            impl Pinned { pub fn add(amount: i64) -> i64 { todo!() } }
        }",
    ));
    assert!(
        generated.contains("impl :: core :: default :: Default for Balance { fn default () -> Self { Self { total : :: core :: default :: Default :: default () , # [cfg (feature = \"audit\")] audited : :: core :: default :: Default :: default () } } }"),
        "{generated}"
    );
}

/// The `Default` the macro writes for a state is written before the compiler evaluates
/// `#[cfg_attr(..)]`, and follows only a plain `#[cfg(..)]` on a field: a field under
/// `#[cfg_attr(.., cfg(..))]` would be named in every configuration, so it is refused there. A
/// `Default` the author derives or implements follows the attribute, and the same state without it
/// expands.
#[test]
fn the_default_the_macro_writes_refuses_a_field_under_cfg_attr_cfg() {
    let module = |default: &str, condition: &str| {
        format!(
            "mod ledger {{
                #[state]
                {default}
                pub struct Balance {{
                    pub total: i64,
                    {condition}
                    pub audited: u32,
                }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ todo!() }} }}
            }}"
        )
    };
    let condition = "#[cfg_attr(test, cfg(feature = \"audit\"))]";
    let error = refused("example:ledger@1.0.0", &module("", condition));
    assert!(
        error.contains("`#[cfg_attr(..)]` wrapping `#[cfg(..)]` on the field `Balance::audited`: the macro writes the `Default` of the state `Balance` before the compiler evaluates the attribute, so that `Default` would name `Balance::audited` in every configuration; write the condition as a plain `#[cfg(..)]`, which the macro carries onto the field's initializer, or derive or implement `Default`"),
        "{error}"
    );
    expanded(
        "example:ledger@1.0.0",
        &module("#[derive(Default)]", condition),
    );
    let authored = module("", condition).replace(
        "#[service]",
        "impl Default for Balance { fn default() -> Self { todo!() } }
        #[service]",
    );
    expanded("example:ledger@1.0.0", &authored);
    expanded("example:ledger@1.0.0", &module("", ""));
}

/// The macro writes the `Default` of a state whose `Default` the author neither derives nor
/// implements, before the compiler evaluates `#[cfg_attr(..)]`: a derive of `Default` under one,
/// directly, under another name or through another `#[cfg_attr(..)]`, would give the state a
/// second `Default` where the attribute applies, so it is refused. A `#[cfg_attr(..)]` deriving
/// another trait expands, and so do a derive under one beside an `impl Default` the author writes
/// for the other configurations, and the same state without the attribute.
#[test]
fn the_default_the_macro_writes_refuses_a_derive_of_default_under_cfg_attr() {
    let module = |attributes: &str| {
        format!(
            "mod ledger {{
                #[state]
                {attributes}
                pub struct Balance {{ pub total: i64 }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ todo!() }} }}
            }}"
        )
    };
    for attributes in [
        "#[cfg_attr(feature = \"audit\", derive(Default))]",
        "#[cfg_attr(test, derive(Debug, Default))]",
        "#[cfg_attr(test, cfg_attr(feature = \"audit\", derive(Default)))]",
        "#[cfg_attr(feature = \"audit\", derive(smart_default::SmartDefault))]",
        "#[cfg_attr(feature = \"audit\", derive(educe::Educe), educe(Debug, Default))]",
    ] {
        let error = refused("example:ledger@1.0.0", &module(attributes));
        assert!(
            error.contains("`#[cfg_attr(..)]` deriving `Default` on the state `Balance`: the macro writes the `Default` of the state `Balance` before the compiler evaluates the attribute, so `Balance` would implement `Default` twice where the attribute applies; derive `Default` without `#[cfg_attr(..)]`, or implement it"),
            "{attributes}: {error}"
        );
    }
    expanded(
        "example:ledger@1.0.0",
        &module("#[cfg_attr(feature = \"audit\", derive(Debug))]"),
    );
    let paired = module("#[cfg_attr(feature = \"audit\", derive(Default))]").replace(
        "#[service]",
        "#[cfg(not(feature = \"audit\"))]
        impl Default for Balance { fn default() -> Self { Balance { total: 7 } } }
        #[service]",
    );
    expanded("example:ledger@1.0.0", &paired);
    expanded("example:ledger@1.0.0", &module(""));
}

/// The macro derives `serde`'s `Serialize` and `Deserialize` for a state kept out of the WIT, those
/// the author does not derive, before the compiler evaluates `#[cfg_attr(..)]`: a derive of either
/// under one, directly or through another `#[cfg_attr(..)]`, through any path the macro follows to
/// `serde`, or a `Serialize` or `Deserialize` it cannot follow to a crate, brought by a glob import
/// or from above the Orb module, which it takes for `serde`'s as on a type entering the WIT, would
/// give the state a second implementation where the attribute applies, so it is refused, whoever
/// gives the state its `Default`. Another crate's derive of either name, `rkyv::Serialize`, and a
/// `#[cfg_attr(..)]` deriving another trait expand, and so does the same state without the
/// attribute.
#[test]
fn a_serde_derive_under_cfg_attr_on_a_state_out_of_the_wit_is_refused() {
    let module = |uses: &str, attributes: &str| {
        format!(
            "mod ledger {{
                {uses}
                #[state]
                {attributes}
                pub struct Balance {{ pub total: i64 }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ todo!() }} }}
            }}"
        )
    };
    let named = "use serde::Serialize;";
    for (uses, attributes, name) in [
        (
            named,
            "#[cfg_attr(feature = \"audit\", derive(serde::Serialize))]",
            "Serialize",
        ),
        (
            named,
            "#[cfg_attr(test, derive(Debug, serde::Deserialize))]",
            "Deserialize",
        ),
        (
            named,
            "#[cfg_attr(test, cfg_attr(feature = \"audit\", derive(::serde::Serialize)))]",
            "Serialize",
        ),
        (
            named,
            "#[cfg_attr(feature = \"audit\", derive(Serialize))]",
            "Serialize",
        ),
        (
            named,
            "#[cfg_attr(feature = \"audit\", derive(serde_derive::Deserialize))]",
            "Deserialize",
        ),
        (
            named,
            "#[cfg_attr(feature = \"audit\", derive(crate::prelude::Serialize))]",
            "Serialize",
        ),
        (
            named,
            "#[derive(Default)] #[cfg_attr(feature = \"audit\", derive(serde::Serialize))]",
            "Serialize",
        ),
        (
            named,
            "#[derive(serde::Serialize)] #[cfg_attr(feature = \"audit\", derive(serde::Deserialize))]",
            "Deserialize",
        ),
        (
            "use serde::*;",
            "#[cfg_attr(feature = \"audit\", derive(Serialize))]",
            "Serialize",
        ),
        (
            "",
            "#[cfg_attr(feature = \"audit\", derive(super::Deserialize))]",
            "Deserialize",
        ),
    ] {
        let error = refused("example:ledger@1.0.0", &module(uses, attributes));
        assert!(
            error.contains(&format!("`#[cfg_attr(..)]` deriving `{name}` on the state `Balance`: the macro derives `serde`'s `{name}` for the state `Balance` before the compiler evaluates the attribute, so `Balance` would implement `{name}` twice where the attribute applies; derive `{name}` without `#[cfg_attr(..)]`, or remove the derive, which the macro writes in every configuration")),
            "{uses} {attributes}: {error}"
        );
    }
    for attributes in [
        "#[cfg_attr(feature = \"audit\", derive(rkyv::Serialize))]",
        "#[cfg_attr(feature = \"audit\", derive(Debug))]",
        "",
    ] {
        expanded("example:ledger@1.0.0", &module(named, attributes));
    }
}

/// The macro derives `serde`'s traits as well for every type of the module that a state kept out
/// of the WIT reaches through its fields, directly or through other types, and that stays out too:
/// a derive of either under `#[cfg_attr(..)]` on such a struct or enum is refused as on the state.
/// The same derive on a type no state reaches, which the macro gives no `serde`, expands, and so
/// do the same types without the attribute.
#[test]
fn a_serde_derive_under_cfg_attr_on_a_type_a_state_reaches_is_refused() {
    let module = |note: &str, mood: &str, other: &str| {
        format!(
            "mod ledger {{
                #[state]
                pub struct Balance {{ pub total: i64, pub notes: Vec<Note> }}
                {note}
                pub struct Note {{ pub text: String, pub mood: Mood }}
                {mood}
                pub enum Mood {{ Calm, Loud }}
                {other}
                pub struct Other {{ pub n: u32 }}
                #[service]
                pub struct Pinned;
                impl Pinned {{ pub fn add(amount: i64) -> i64 {{ todo!() }} }}
            }}"
        )
    };
    let attribute = "#[cfg_attr(feature = \"note\", derive(serde::Serialize))]";
    for (source, owner) in [
        (module(attribute, "", ""), "Note"),
        (module("", attribute, ""), "Mood"),
    ] {
        let error = refused("example:ledger@1.0.0", &source);
        assert!(
            error.contains(&format!("`#[cfg_attr(..)]` deriving `Serialize` on `{owner}`: the macro derives `serde`'s `Serialize` for `{owner}`, which a state's fields reach, before the compiler evaluates the attribute, so `{owner}` would implement `Serialize` twice where the attribute applies; derive `Serialize` without `#[cfg_attr(..)]`, or remove the derive, which the macro writes in every configuration")),
            "{owner}: {error}"
        );
    }
    expanded("example:ledger@1.0.0", &module("", "", attribute));
    expanded("example:ledger@1.0.0", &module("", "", ""));
}

const GATE: &str = "mod gate {
    /// Each contributor may change the admission or block it.
    #[hook(composition = waterfall, input = Admission)]
    pub struct Guard;
    /// One admission.
    pub struct Admission { pub value: i32, pub blocked: bool }
    #[service]
    pub struct Admissions;
    impl Admissions {
        pub fn admit(value: i32) -> Admission { Guard::run(Admission { value, blocked: false }) }
    }
}";

/// A hook declared from `input = Type` is an interface of the package, derived from the struct:
/// its types are defined there and borrowed by the services, the world imports and exports it,
/// the manifest declares it, and `run` speaks the module's own types through generated
/// conversions to the import side of the bindings.
#[test]
fn a_declared_hook_is_derived_from_its_input() {
    let module = expanded("example:gate@1.0.0", GATE);
    assert_eq!(
        wit(&module),
        "package example:gate@1.0.0;\n\n/// Each contributor may change the admission or block it.\ninterface guard {\n  /// One admission.\n  record admission {\n    value: s32,\n    blocked: bool,\n  }\n  apply: func(input: admission) -> admission;\n}\n\ninterface admissions {\n  use guard.{admission};\n  admit: func(value: s32) -> admission;\n}\n\nworld gate {\n  import guard;\n  export guard;\n  export admissions;\n}\n"
    );
    assert_eq!(
        manifest(&module).as_deref(),
        Some(
            r#"{"contributes":{},"declares":{"example:gate/guard@1.0.0":"waterfall"},"services":{}}"#
        )
    );
    let generated = tokens(&module);
    for expected in [
        "pub use exports :: example :: gate :: guard :: { Admission } ;",
        "pub fn run (input : Admission) -> Admission",
        "let input : self :: example :: gate :: guard :: Admission = :: core :: convert :: Into :: into (input) ;",
        "let answer = self :: example :: gate :: guard :: apply (input) ;",
        "impl :: core :: convert :: From < exports :: example :: gate :: guard :: Admission > for self :: example :: gate :: guard :: Admission",
        "impl :: core :: convert :: From < self :: example :: gate :: guard :: Admission > for exports :: example :: gate :: guard :: Admission",
        "Self { value : :: core :: convert :: Into :: into (value . value) , blocked : :: core :: convert :: Into :: into (value . blocked) }",
        "fn apply (_input : Admission) -> Admission",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    assert!(
        !generated.contains("pub use self :: example :: gate :: guard ;"),
        "{generated}"
    );
}

/// The composition shapes the callback: a series hook returns nothing, a bail hook an option.
#[test]
fn a_derived_hook_follows_its_composition() {
    let series = expanded(
        "example:gate@1.0.0",
        &GATE.replace("composition = waterfall", "composition = series"),
    );
    assert!(
        wit(&series).contains("  apply: func(input: admission);\n"),
        "{}",
        wit(&series)
    );
    assert!(
        tokens(&series).contains("pub fn run (input : Admission) {"),
        "{}",
        tokens(&series)
    );
    let bail = expanded(
        "example:gate@1.0.0",
        &GATE.replace("composition = waterfall", "composition = bail"),
    );
    assert!(
        wit(&bail).contains("  apply: func(input: admission) -> option<admission>;\n"),
        "{}",
        wit(&bail)
    );
    assert!(
        tokens(&bail).contains(
            "pub fn run (input : Admission) -> :: core :: option :: Option < Admission >"
        ),
        "{}",
        tokens(&bail)
    );
    assert!(
        tokens(&bail).contains("answer . map (| item | :: core :: convert :: Into :: into (item))"),
        "{}",
        tokens(&bail)
    );
}

/// A scalar input needs no record and no conversion; a list crosses to the import by reference;
/// the shapes a hook reaches, through lists, options and variants, get conversions each.
#[test]
fn a_derived_hook_converts_what_its_input_reaches() {
    let scalar = expanded(
        "example:gate@1.0.0",
        &GATE
            .replace("input = Admission", "input = i32")
            .replace(
                "pub fn admit(value: i32) -> Admission { Guard::run(Admission { value, blocked: false }) }",
                "pub fn admit(value: i32) -> i32 { Guard::run(value) }",
            ),
    );
    assert!(
        wit(&scalar).contains("interface guard {\n  apply: func(input: s32) -> s32;\n}"),
        "{}",
        wit(&scalar)
    );
    assert!(!wit(&scalar).contains("use guard"), "{}", wit(&scalar));
    assert!(!tokens(&scalar).contains("convert :: From"));
    let list = expanded(
        "example:gate@1.0.0",
        &GATE.replace("input = Admission", "input = Vec<Admission>"),
    );
    assert!(
        wit(&list).contains("apply: func(input: list<admission>) -> list<admission>;"),
        "{}",
        wit(&list)
    );
    assert!(
        tokens(&list).contains("self :: example :: gate :: guard :: apply (& input)"),
        "{}",
        tokens(&list)
    );
    let nested = expanded(
        "example:gate@1.0.0",
        "mod gate {
            #[hook(composition = waterfall, input = Admission)]
            pub struct Guard;
            pub struct Admission { pub tags: Vec<Tag>, pub note: Option<Note>, pub kind: Kind }
            pub enum Tag { Hot, Cold }
            pub enum Kind { Plain, Weighted(u32) }
            pub struct Note { pub text: String }
            #[service]
            pub struct Admissions;
            impl Admissions { pub fn count() -> u32 { 0 } }
        }",
    );
    let text = wit(&nested);
    for expected in [
        "  record admission {\n",
        "  enum tag {\n",
        "  variant kind {\n",
        "  record note {\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(
        text.contains("interface admissions {\n  count: func() -> u32;\n}"),
        "{text}"
    );
    let generated = tokens(&nested);
    for expected in [
        "From < exports :: example :: gate :: guard :: Tag > for self :: example :: gate :: guard :: Tag",
        "From < exports :: example :: gate :: guard :: Note > for self :: example :: gate :: guard :: Note",
        "value . tags . into_iter () . map (| item | :: core :: convert :: Into :: into (item)) . collect ()",
        "value . note . map (| item | :: core :: convert :: Into :: into (item))",
        "Kind :: Weighted (item) => Self :: Weighted (:: core :: convert :: Into :: into (item))",
        "Tag :: Hot => Self :: Hot",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// A declaration names an existing interface or derives one, never both or neither; its input is a
/// type of the module or a scalar, string or list; its name is free of the world's and the services'.
#[test]
fn a_derived_hook_is_refused_when_ill_formed() {
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace(
            "#[hook(composition = waterfall, input = Admission)]",
            "#[hook(\"example:admission/guard@1.0.0\", composition = waterfall, input = Admission)]",
        ),
    );
    assert!(error.contains("not both"), "{error}");
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace(", input = Admission", ""),
    );
    assert!(
        error.contains("names an existing interface or its input type"),
        "{error}"
    );
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace("input = Admission", "input = Missing"),
    );
    assert!(
        error.contains("is not a struct or enum of this module"),
        "{error}"
    );
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace("input = Admission", "input = Option<Admission>"),
    );
    assert!(error.contains("a hook's input is a record"), "{error}");
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace("pub struct Guard;", "pub struct Gate;"),
    );
    assert!(error.contains("also the name of the world"), "{error}");
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace("pub struct Guard;", "pub struct Admissions;"),
    );
    assert!(
        error.contains("names both a hook and the service"),
        "{error}"
    );
    let error = refused(
        "example:gate@1.0.0",
        &GATE.replace("pub struct Guard;", "pub struct Guard { pub n: u32 }"),
    );
    assert!(
        error.contains("a hook declaration is a unit struct"),
        "{error}"
    );
}

const ADMISSION_WIT: &str = "package example:admission@1.0.0;
interface guard {
  record admission { value: s32, blocked: bool }
  apply: func(input: admission) -> admission;
}
";

const SHARED_GATE: &str = "mod gate {
    #[hook(\"example:admission/guard@1.0.0\", composition = waterfall)]
    pub struct Guard;
    #[service]
    pub struct Admissions;
    impl Admissions {
        pub fn admit(value: i32) -> i32 { Guard::run(guard::Admission { value, blocked: false }).value }
    }
}";

/// A hook declared from a consumed package keeps that package's types: the world imports and
/// exports the identity, the import module is re-exported under the interface's name and `run`
/// takes its types; the composition must fit the callback.
#[test]
fn a_declared_hook_may_name_a_consumed_interface() {
    let dir = consumed("admission", ADMISSION_WIT);
    let module = orb_with(options("example:gate@1.0.0", Some(&dir)), SHARED_GATE)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        wit(&module).contains(
            "world gate {\n  import example:admission/guard@1.0.0;\n  export example:admission/guard@1.0.0;\n  export admissions;\n}"
        ),
        "{}",
        wit(&module)
    );
    let generated = tokens(&module);
    assert!(
        generated.contains("pub use self :: example :: admission :: guard ;"),
        "{generated}"
    );
    assert!(
        generated.contains("pub fn run (input : self :: example :: admission :: guard :: Admission) -> self :: example :: admission :: guard :: Admission"),
        "{generated}"
    );
    let error = refused_with(
        options("example:gate@1.0.0", Some(&dir)),
        &SHARED_GATE.replace("composition = waterfall", "composition = series"),
    );
    assert!(error.contains("does not fit a series hook"), "{error}");
    let error = refused("example:gate@1.0.0", SHARED_GATE);
    assert!(error.contains("hooks need wit ="), "{error}");
}

/// `run` hands the hook's import its input the way the bindings take it: by reference for a
/// string, a list, or a record or variant that holds one anywhere, by value otherwise; for a hook
/// derived from the module and for one named from a consumed package alike.
#[test]
fn run_borrows_an_input_that_holds_a_string_or_a_list() {
    let derived = |types: &str| {
        tokens(&expanded(
            "example:gate@1.0.0",
            &GATE.replace(
                "pub struct Admission { pub value: i32, pub blocked: bool }",
                types,
            ),
        ))
    };
    let borrowed = "self :: example :: gate :: guard :: apply (& input)";
    let owned = "self :: example :: gate :: guard :: apply (input)";
    for (types, by_reference) in [
        (
            "pub struct Admission { pub value: i32, pub blocked: bool }",
            false,
        ),
        (
            "pub struct Admission { pub value: i32, pub blocked: bool, pub note: String }",
            true,
        ),
        (
            "pub struct Admission { pub value: i32, pub blocked: bool, pub detail: Option<Detail> }
            pub struct Detail { pub tags: Vec<u32> }",
            true,
        ),
        (
            "pub struct Admission { pub value: i32, pub blocked: bool, pub kind: Kind }
            pub enum Kind { Plain, Named(String) }",
            true,
        ),
    ] {
        let generated = derived(types);
        let expected = if by_reference { borrowed } else { owned };
        assert!(generated.contains(expected), "{types}\n{generated}");
    }
    let consumed_run = |name: &str, record: &str| {
        let dir = consumed(
            name,
            &ADMISSION_WIT.replace("record admission { value: s32, blocked: bool }", record),
        );
        tokens(
            &orb_with(options("example:gate@1.0.0", Some(&dir)), SHARED_GATE)
                .unwrap_or_else(|error| panic!("{error}")),
        )
    };
    let plain = consumed_run("plain", "record admission { value: s32, blocked: bool }");
    assert!(
        plain.contains("{ self :: example :: admission :: guard :: apply (input) }"),
        "{plain}"
    );
    let noted = consumed_run(
        "noted",
        "record admission { value: s32, blocked: bool, notes: list<string> }",
    );
    assert!(
        noted.contains("{ self :: example :: admission :: guard :: apply (& input) }"),
        "{noted}"
    );
}

/// An input that is an option, a result or a tuple over strings, lists or records holding one is
/// lent piece by piece, as the bindings borrow it: a string as `&str`, a list as `&[T]`, a record
/// as `&T`, and the other parts copied; an input holding no string or list crosses by value.
#[test]
fn run_lends_the_parts_of_an_option_a_result_or_a_tuple() {
    let run = |name: &str, input: &str| {
        let dir = consumed(
            name,
            &format!(
                "package example:admission@1.0.0;
                interface guard {{
                  record note {{ text: string }}
                  apply: func(input: {input}) -> {input};
                }}"
            ),
        );
        let module = orb_with(
            options("example:gate@1.0.0", Some(&dir)),
            "mod gate {
                #[hook(\"example:admission/guard@1.0.0\", composition = waterfall)]
                pub struct Guard;
                #[service]
                pub struct Admissions;
                impl Admissions { pub fn count() -> u32 { 0 } }
            }",
        )
        .unwrap_or_else(|error| panic!("{input}: {error}"));
        let generated = tokens(&module);
        let call = "self :: example :: admission :: guard :: apply (";
        let start = generated.find(call).expect("the call") + call.len();
        generated[start..].to_owned()
    };
    let some = ":: core :: option :: Option :: Some";
    let none = ":: core :: option :: Option :: None";
    let ok = ":: core :: result :: Result :: Ok";
    let err = ":: core :: result :: Result :: Err";
    for (name, input, expected) in [
        (
            "lent-option",
            "option<string>",
            format!(
                "match & input {{ {some} (item) => {some} (item . as_str ()) , {none} => {none} , }})"
            ),
        ),
        (
            "lent-result",
            "result<list<u8>, string>",
            format!(
                "match & input {{ {ok} (item) => {ok} (item . as_slice ()) , {err} (item) => {err} (item . as_str ()) , }})"
            ),
        ),
        (
            "lent-tuple",
            "tuple<option<note>, u64, result<_, string>>",
            format!(
                "{{ let (item0 , item1 , item2 ,) = & input ; (match item0 {{ {some} (item) => {some} (item) , {none} => {none} , }} , :: core :: clone :: Clone :: clone (item1) , match item2 {{ {ok} (item) => {ok} (:: core :: clone :: Clone :: clone (item)) , {err} (item) => {err} (item . as_str ()) , }} ,) }})"
            ),
        ),
        ("lent-plain", "option<u32>", "input)".to_owned()),
    ] {
        let lent = run(name, input);
        assert!(lent.starts_with(&expected), "{input}: {expected}\n{lent}");
    }
}

/// A waterfall callback returns its input type, through aliases and anonymous types alike, as
/// the kernel compares them: `type out = admission` returns an admission, `list<u32>` is one
/// type wherever it is written; another record is not the input type.
#[test]
fn a_waterfall_result_is_its_input_type_through_aliases() {
    let waterfall = |name: &str, callback: &str| {
        let dir = consumed(
            name,
            &ADMISSION_WIT.replace("  apply: func(input: admission) -> admission;", callback),
        );
        orb_with(options("example:gate@1.0.0", Some(&dir)), SHARED_GATE)
    };
    let module = waterfall(
        "aliased",
        "  type out = admission;\n  apply: func(input: admission) -> out;",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        tokens(&module).contains("pub fn run (input : self :: example :: admission :: guard :: Admission) -> self :: example :: admission :: guard :: Out"),
        "{}",
        tokens(&module)
    );
    for (name, callback) in [
        (
            "doubled",
            "  type entry = admission;\n  type out = entry;\n  apply: func(input: entry) -> out;",
        ),
        (
            "listed",
            "  type batch = list<admission>;\n  apply: func(input: list<admission>) -> batch;",
        ),
        (
            "optional",
            "  apply: func(input: option<tuple<admission, u32>>) -> option<tuple<admission, u32>>;",
        ),
    ] {
        let accepted = waterfall(name, callback);
        // The module's own `run` speaks the old input, so a compile error about it is expected;
        // the composition itself is accepted.
        if let Err(error) = accepted {
            assert!(
                !error.to_string().contains("does not fit a waterfall hook"),
                "{name}: {error}"
            );
        }
    }
    let error = waterfall(
        "unlike",
        "  record other { value: s32, blocked: bool }\n  apply: func(input: admission) -> other;",
    )
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(
        error
            .contains("does not fit a waterfall hook: a waterfall callback returns its input type"),
        "{error}"
    );
}

const LEDGER: &str = "mod ledger {
    pub mod pinned {
        /// The balance of a pinned ledger object.
        #[state]
        pub struct Balance { pub total: i64 }
        /// A ledger pinned until the host closes.
        #[service(lifetime = host)]
        pub struct Pinned;
        impl Pinned {
            /// Add to the balance and return it.
            pub fn add(amount: i64) -> Balance { Balance::update(|balance| balance.total += amount) }
        }
    }
    pub mod passing {
        #[state]
        pub struct Balance { pub total: i64 }
        #[service]
        pub struct Passing;
        impl Passing {
            pub fn add(amount: i64) -> i64 { Balance::update(|balance| balance.total += amount).total }
        }
    }
}";

/// A sub-module groups a service with its state: the state needs no `service = Name`, two states
/// of two modules may share a name when they stay out of the WIT, the generated types are
/// re-exported where the author declared them, and the bridges are generated in the service's
/// module, naming the Orb module through `super::`.
#[test]
fn sub_modules_group_a_service_with_its_state() {
    let module = expanded("example:ledger@1.0.0", LEDGER);
    assert_eq!(
        wit(&module),
        "package example:ledger@1.0.0;\n\n/// A ledger pinned until the host closes.\ninterface pinned {\n  /// The balance of a pinned ledger object.\n  record balance {\n    total: s64,\n  }\n  /// Add to the balance and return it.\n  add: func(amount: s64) -> balance;\n}\n\ninterface passing {\n  add: func(amount: s64) -> s64;\n}\n\nworld ledger {\n  import tembra:guest/state@1.0.0;\n  export tembra:guest/state-init@1.0.0;\n  export pinned;\n  export passing;\n}\n"
    );
    assert_eq!(
        manifest(&module).as_deref(),
        Some(
            r#"{"contributes":{},"declares":{},"services":{"example:ledger/pinned@1.0.0":{"lifetime":"host","linger_ms":0}}}"#
        )
    );
    let generated = tokens(&module);
    for expected in [
        "pub use super :: exports :: example :: ledger :: pinned :: { Balance } ;",
        "impl super :: exports :: example :: ledger :: pinned :: Guest for super :: __TembraExports { fn add (amount : i64) -> Balance { Pinned :: add (amount) } }",
        "impl super :: exports :: example :: ledger :: passing :: Guest for super :: __TembraExports { fn add (amount : i64) -> i64 { Passing :: add (amount) } }",
        "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Balance { pub total : i64 }",
        "impl pinned :: Balance {",
        "impl passing :: Balance {",
        "< pinned :: Balance as :: core :: default :: Default > :: default ()",
        "< passing :: Balance as :: core :: default :: Default > :: default ()",
        "\"example:ledger/pinned@1.0.0\" => :: core :: option :: Option :: Some (",
        "\"example:ledger/passing@1.0.0\" => :: core :: option :: Option :: Some (",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// The owner of a state is the one service of its module, else the one service of the Orb; a file
/// module cannot be read; an item the Orb module uses must be visible to it; a name that enters the
/// WIT is one across the modules.
#[test]
fn sub_modules_are_refused_when_ill_formed() {
    let error = refused(
        "example:ledger@1.0.0",
        &LEDGER.replace(
            "        #[service]\n        pub struct Passing;",
            "        #[service]\n        pub struct Passing;\n        #[service]\n        pub struct Other;\n        impl Other { pub fn count() -> u32 { 0 } }",
        ),
    );
    assert!(
        error.contains("put the state in the service's sub-module"),
        "{error}"
    );
    let error = refused(
        "example:ledger@1.0.0",
        &LEDGER.replace(
            "    pub mod passing {",
            "    mod elsewhere;\n    pub mod passing {",
        ),
    );
    assert!(error.contains("written inline"), "{error}");
    let error = refused(
        "example:ledger@1.0.0",
        &LEDGER.replace(
            "        #[state]\n        pub struct Balance { pub total: i64 }\n        #[service]",
            "        #[state]\n        pub struct Balance { total: i64 }\n        #[service]",
        ),
    );
    assert!(
        error.contains("make the struct and its fields visible to the Orb module"),
        "{error}"
    );
    let error = refused(
        "example:ledger@1.0.0",
        &LEDGER.replace(
            "        impl Passing {",
            "        impl Balance { #[validate] fn check(&self) -> Result<(), String> { Ok(()) } }\n        impl Passing {",
        ),
    );
    assert!(
        error.contains("#[validate] on a function of a sub-module: make it visible"),
        "{error}"
    );
    let error = refused(
        "example:ledger@1.0.0",
        &LEDGER.replace(
            "pub fn add(amount: i64) -> i64 { Balance::update(|balance| balance.total += amount).total }",
            "pub fn add(amount: i64) -> Balance { Balance::update(|balance| balance.total += amount) }",
        ),
    );
    assert!(
        error.contains("`Balance` is declared in `pinned` and in `passing`"),
        "{error}"
    );
}

/// A name resolves in the module that writes it first: a record named like a resource of another
/// module is the record where its own module names it, in a parameter, a result or a hook's
/// input, so the two are refused for sharing a name that enters the WIT, not read as the resource.
#[test]
fn a_name_resolves_in_the_module_that_writes_it_first() {
    let desk = |record: &str, tickets: &str| {
        format!(
            "mod desk {{
                pub mod tickets {{
                    pub struct {record} {{ pub id: u32 }}
                    #[service]
                    pub struct Tickets;
                    {tickets}
                }}
                pub mod sessions {{
                    #[wit_resource]
                    pub struct Token;
                    impl Token {{ pub fn id(&self) -> u32 {{ 0 }} }}
                    #[service]
                    pub struct Sessions;
                    impl Sessions {{ pub fn open() -> Token {{ Token }} }}
                }}
            }}"
        )
    };
    for tickets in [
        "impl Tickets { pub fn check(token: Token) -> u32 { token.id } }",
        "impl Tickets { pub fn issue() -> Token { Token { id: 1 } } }",
        "#[hook(composition = series, input = Token)]
        pub struct Guard;
        impl Tickets { pub fn count() -> u32 { 0 } }",
    ] {
        let error = refused("example:desk@1.0.0", &desk("Token", tickets));
        assert!(
            error.contains("`Token` is declared in `tickets` and in `sessions`; a name that enters the WIT is one across the Orb module"),
            "{tickets}: {error}"
        );
        expanded(
            "example:desk@1.0.0",
            &desk("Stamp", &tickets.replace("Token", "Stamp")),
        );
    }
}

/// A type of any shape takes its name in the module declaring it before a resource of another
/// module: a tuple struct, a unit struct, a type alias or a union named like the resource is
/// refused as a type of the module that cannot enter the WIT, whichever interface the resource
/// belongs to. An alias whose path ends in its own name stands for that path, as a `use` of it
/// does, so an alias of the resource is the resource.
#[test]
fn a_type_the_module_declares_takes_its_name_before_a_resource() {
    let desk = |declared: &str, owner: &str| {
        format!(
            "mod desk {{
                pub mod tickets {{
                    {declared}
                    #[service]
                    pub struct Tickets;
                    impl Tickets {{ pub fn check(token: Token) -> u32 {{ 0 }} }}
                }}
                pub mod sessions {{
                    #[wit_resource(service = {owner})]
                    pub struct Token;
                    impl Token {{ pub fn id(&self) -> u32 {{ 0 }} }}
                    #[service]
                    pub struct Sessions;
                    impl Sessions {{ pub fn count() -> u32 {{ 0 }} }}
                }}
            }}"
        )
    };
    for declared in [
        "pub struct Token(pub u32);",
        "pub struct Token;",
        "pub type Token = u32;",
        "pub union Token { pub id: u32 }",
    ] {
        for owner in ["Sessions", "Tickets"] {
            let error = refused("example:desk@1.0.0", &desk(declared, owner));
            assert!(
                error.contains("`Token` is used by a service method or a hook but is not a struct or enum of this module"),
                "{declared}, a resource of `{owner}`: {error}"
            );
        }
    }
    let alias = "pub type Token = super::sessions::Token;";
    let error = refused("example:desk@1.0.0", &desk(alias, "Sessions"));
    assert!(
        error.contains("`Tickets::check` takes `Token`, a resource of the interface `sessions`"),
        "{error}"
    );
    expanded("example:desk@1.0.0", &desk(alias, "Tickets"));
}

/// A path into another module of the Orb names its type there, as in Rust: `super::sessions::Token`
/// or `super::model::Entry`, or `sessions::Token` after `use super::sessions;` or
/// `use super::sessions as s;`, written in a module declaring a `Token` or an `Entry` of its own,
/// in a signature or in a record's field, is the other module's resource or record, which the bare
/// name would not be, so the function takes the resource or returns the record as it would after a
/// `use` of the type.
#[test]
fn a_path_into_another_module_names_its_type_there() {
    let desk = |uses: &str, token: &str| {
        format!(
            "mod desk {{
                pub mod tickets {{
                    {uses}
                    pub struct Token(pub u32);
                    #[service]
                    pub struct Tickets;
                    impl Tickets {{ pub fn check(token: {token}) -> u32 {{ 0 }} }}
                }}
                pub mod sessions {{
                    #[wit_resource(service = Tickets)]
                    pub struct Token;
                    impl Token {{ pub fn id(&self) -> u32 {{ 0 }} }}
                    #[service]
                    pub struct Sessions;
                    impl Sessions {{ pub fn count() -> u32 {{ 0 }} }}
                }}
            }}"
        )
    };
    for (uses, token) in [
        ("", "super::sessions::Token"),
        ("use super::sessions;", "sessions::Token"),
        ("use super::sessions as s;", "s::Token"),
    ] {
        let text = resolved(&expanded("example:desk@1.0.0", &desk(uses, token)));
        assert!(
            text.contains("  check: func(token: token) -> u32;\n"),
            "{uses} {token}\n{text}"
        );
    }
    let books = |api: &str| {
        format!(
            "mod books {{
                pub mod model {{ pub struct Entry {{ pub id: u32 }} }}
                pub mod api {{
                    pub struct Entry(pub u32);
                    {api}
                    #[service]
                    pub struct Shelf;
                }}
            }}"
        )
    };
    let entry = "  record entry {\n    id: u32,\n  }\n";
    let text = resolved(&expanded(
        "example:books@1.0.0",
        &books("impl Shelf { pub fn first() -> super::model::Entry { todo!() } }"),
    ));
    for expected in [entry, "  first: func() -> entry;\n"] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let text = resolved(&expanded(
        "example:books@1.0.0",
        &books(
            "pub struct Stack { pub top: super::model::Entry }
            impl Shelf { pub fn first() -> Stack { todo!() } }",
        ),
    ));
    for expected in [
        entry,
        "  record stack {\n    top: entry,\n  }\n",
        "  first: func() -> stack;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

/// A type the module declares takes its name there wherever the module writes it, as in Rust: a
/// tuple struct named like another module's record, in a parameter or in a record's field, is
/// refused as a type of the module that cannot enter the WIT, not read as that record, and so is
/// one in a field named like another module's resource. A module that declares no type of the
/// name, importing it, still takes the other module's type.
#[test]
fn a_type_the_module_declares_takes_its_name_before_another_modules_record() {
    let desk = |tickets: &str, shelves: &str| {
        format!(
            "mod desk {{
                pub mod tickets {{
                    {tickets}
                    #[service]
                    pub struct Tickets;
                }}
                pub mod shelves {{
                    {shelves}
                    #[service]
                    pub struct Shelves;
                    impl Shelves {{ pub fn count() -> u32 {{ 0 }} }}
                }}
            }}"
        )
    };
    let record = "pub struct Token { pub id: u32 }";
    let resource = "#[wit_resource(service = Tickets)]
        pub struct Token { id: u32 }
        impl Token { pub fn id(&self) -> u32 { self.id } }";
    let parameter = "impl Tickets { pub fn check(token: Token) -> u32 { 0 } }";
    let field = "pub struct Pass { pub token: Token }
        impl Tickets { pub fn check(pass: Pass) -> bool { true } }";
    for (uses, shelves) in [(parameter, record), (field, record), (field, resource)] {
        let error = refused(
            "example:desk@1.0.0",
            &desk(&format!("pub struct Token(pub u32);\n{uses}"), shelves),
        );
        assert!(
            error.contains("`Token` is used by a service method or a hook but is not a struct or enum of this module"),
            "{uses}\n{shelves}: {error}"
        );
        expanded(
            "example:desk@1.0.0",
            &desk(&format!("use super::shelves::Token;\n{uses}"), shelves),
        );
    }
    let text = wit(&expanded(
        "example:desk@1.0.0",
        &desk(&format!("use super::shelves::Token;\n{parameter}"), record),
    ));
    for expected in [
        "  record token {\n    id: u32,\n  }\n",
        "  check: func(token: token) -> u32;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

/// Two resources of one name in other modules are as ambiguous as two records: a function of a
/// third module taking that name is refused, whichever of the two the module imports, and expands
/// once one of them takes another name.
#[test]
fn two_resources_of_one_name_in_other_modules_are_ambiguous() {
    let desk = |second: &str| {
        format!(
            "mod desk {{
                pub mod a {{
                    #[wit_resource(service = Tickets)]
                    pub struct Token;
                    impl Token {{ pub fn id(&self) -> u32 {{ 0 }} }}
                }}
                pub mod b {{
                    #[wit_resource(service = Tickets)]
                    pub struct {second};
                    impl {second} {{ pub fn id(&self) -> u32 {{ 1 }} }}
                }}
                pub mod c {{
                    use super::a::Token;
                    #[service]
                    pub struct Tickets;
                    impl Tickets {{ pub fn check(token: Token) -> u32 {{ token.id() }} }}
                }}
            }}"
        )
    };
    let error = refused("example:desk@1.0.0", &desk("Token"));
    assert!(
        error.contains("`Token` is declared in several modules of the Orb; a module uses its own `Token`, or the types take different names"),
        "{error}"
    );
    expanded("example:desk@1.0.0", &desk("Pass"));
}

/// Documentation over several lines, a block comment or a `#[doc]` string, is carried line by
/// line into the WIT, each line after its own `///`, so no line of it becomes WIT; a character
/// the WIT lexer refuses is an error on the attribute.
#[test]
fn documentation_over_several_lines_stays_comment_text() {
    let module = expanded(
        PACKAGE,
        "mod book {
        /** The ledger.
        interface injected { surprise: func() -> u32; } */
        #[service]
        pub struct Ledger;
        impl Ledger {
            #[doc = \"How many entries.\\n  type extra = u64;\\r\\nThe end.\"]
            pub fn count() -> u32 { 0 }
        }
    }",
    );
    let text = wit(&module);
    assert!(
        text.contains("\n/// The ledger.\n///        interface injected { surprise: func() -> u32; } \ninterface ledger {\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "  ///How many entries.\n  ///  type extra = u64;\n  ///The end.\n  count: func() -> u32;\n"
        ),
        "{text}"
    );
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_str("book.wit", &text).unwrap();
    let names: Vec<_> = resolve
        .interfaces
        .iter()
        .filter_map(|(_, interface)| interface.name.clone())
        .collect();
    assert_eq!(names, ["ledger"]);
    assert_eq!(resolve.types.len(), 0, "{text}");
    // A block comment's blank opening and closing lines are left out.
    let block = expanded(PACKAGE, &ledger("/**\n The ledger.\n */\n#[service]"));
    assert!(
        wit(&block).contains("\n/// The ledger.\ninterface ledger {\n"),
        "{}",
        wit(&block)
    );
    let error = refused(
        PACKAGE,
        &ledger("#[doc = \"Turned \u{202e} around.\"]\n#[service]"),
    );
    assert!(
        error.contains("this documentation holds U+202E, a character WIT refuses"),
        "{error}"
    );
}

/// Inner attributes, `//!` and `#![..]`, of the Orb module and of a sub-module stay first inside
/// their module's braces; outer ones stay before `mod`.
#[test]
fn inner_attributes_stay_inside_their_module() {
    let module = expanded(
        PACKAGE,
        "/// The book.
        #[allow(unused)]
        mod book {
            //! The Orb module's own documentation.
            #![allow(dead_code)]
            /// The rules.
            pub mod rules {
                //! A sub-module's documentation.
                #![allow(unused_imports)]
                pub fn helper() {}
            }
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }
        }",
    );
    let styles = |module: &ItemMod| -> Vec<(String, bool)> {
        module
            .attrs
            .iter()
            .map(|attribute| {
                let path = attribute.path().get_ident().map(ToString::to_string);
                let inner = matches!(attribute.style, syn::AttrStyle::Inner(_));
                (path.unwrap_or_default(), inner)
            })
            .collect()
    };
    assert_eq!(
        styles(&module),
        [
            ("doc".into(), false),
            ("allow".into(), false),
            ("doc".into(), true),
            ("allow".into(), true)
        ]
    );
    let rules = items(&module)
        .iter()
        .find_map(|item| match item {
            Item::Mod(rules) if rules.ident == "rules" => Some(rules),
            _ => None,
        })
        .expect("the sub-module");
    assert_eq!(
        styles(rules),
        [
            ("doc".into(), false),
            ("doc".into(), true),
            ("allow".into(), true)
        ]
    );
}

/// A contribution in a sub-module names the Orb module's generated modules through `super::`.
#[test]
fn a_contribution_in_a_sub_module_names_the_root_through_super() {
    let dir = consumed("contributor", ADMISSION_WIT);
    let module = orb_with(
        options("example:blocker@1.0.0", Some(&dir)),
        "mod blocker {
            pub mod rules {
                #[hook(\"example:admission/guard@1.0.0\", priority = -7)]
                pub fn block(input: super::guard::Admission) -> super::guard::Admission { input }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    assert!(
        generated.contains("impl super :: exports :: example :: admission :: guard :: Guest for super :: __TembraExports { fn apply (input : super :: guard :: Admission) -> super :: guard :: Admission { block (input) } }"),
        "{generated}"
    );
    assert!(
        generated.contains("pub use exports :: example :: admission :: guard ;"),
        "{generated}"
    );
}

const ACCESS_WIT: &str = "package example:access@1.0.0;
interface grants {
  flags permissions { read, write }
  record grant { who: string, permissions: permissions }
  variant change { granted(grant), revoked(string) }
  record plain { who: string, count: option<u32> }
  resource token;
  record held { tokens: list<option<token>> }
  give: func(grant: grant) -> u32;
  apply: func(change: change) -> u32;
  hold: func(held: held, plain: plain) -> u32;
}
";

/// A state makes the bindings derive serde's traits on the generated types; a record or a variant
/// holding a flags type or a resource handle, anywhere, has no serde implementation to derive, so
/// the derives leave it out, whatever package it comes from.
#[test]
fn the_state_derives_leave_out_the_types_that_cannot_carry_them() {
    let dir = consumed("access", ACCESS_WIT);
    let imports = Options {
        imports: vec![literal("example:access/grants@1.0.0")],
        ..options("example:keeper@1.0.0", Some(&dir))
    };
    let module = orb_with(
        imports,
        "mod vault {
            #[state]
            pub struct Count { pub total: u32 }
            #[service]
            pub struct Keeper;
            impl Keeper { pub fn total() -> u32 { Count::read().total } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    assert!(
        generated.contains("additional_derives : [:: serde :: Serialize , :: serde :: Deserialize] , additional_derives_ignore : [\"change\" , \"grant\" , \"held\"] ,"),
        "{generated}"
    );
    // Without a state the bindings derive nothing more, and nothing is left out.
    let stateless = orb_with(
        Options {
            imports: vec![literal("example:access/grants@1.0.0")],
            ..options("example:keeper@1.0.0", Some(&dir))
        },
        &ledger("#[service]"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        !tokens(&stateless).contains("additional_derives"),
        "{}",
        tokens(&stateless)
    );
}

/// The bindings give a consumed package's flags type no serde implementation, nor a consumed type
/// holding one, and, in an Orb without a state, where they derive serde's traits on no type, no
/// consumed type at all: serde's derives on a type of the module holding such a type are refused
/// at the derive. With a state, one holding a consumed record serde writes leaves them to the
/// bindings.
#[test]
fn a_serde_derive_on_a_type_holding_a_consumed_type_without_serde_is_refused() {
    let dir = consumed("badge", ACCESS_WIT);
    let imports = || Options {
        imports: vec![literal("example:access/grants@1.0.0")],
        ..options("example:keeper@1.0.0", Some(&dir))
    };
    let module = |state: &str, derive: &str, field: &str| {
        format!(
            "mod vault {{
                {state}
                {derive}
                pub struct Badge {{ {field} }}
                #[service]
                pub struct Keeper;
                impl Keeper {{ pub fn issue() -> Badge {{ todo!() }} }}
            }}"
        )
    };
    let state = "#[state] pub struct Count { pub total: u32 }";
    let flags =
        "a consumed package's flags type, which the bindings give no `serde` implementation";
    let stateless = "a type of a consumed package, which the bindings give no `serde` implementation in an Orb without a state";
    for (state, field, held) in [
        (state, "pub permissions: grants::Permissions", Some(flags)),
        (state, "pub grant: Option<grants::Grant>", Some(flags)),
        (state, "pub plain: grants::Plain", None),
        ("", "pub permissions: grants::Permissions", Some(stateless)),
        ("", "pub plain: grants::Plain", Some(stateless)),
        ("", "pub all: Vec<grants::Plain>", Some(stateless)),
    ] {
        let derived = module(state, "#[derive(serde::Serialize)]", field);
        let Some(held) = held else {
            let module = orb_with(imports(), &derived).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(type_attributes(&module), None, "{}", tokens(&module));
            continue;
        };
        let error = refused_with(imports(), &derived);
        assert!(
            error.contains(&format!("`serde::Serialize` on `Badge`: `Badge` enters the WIT and holds {held}, so the generated type replacing it cannot derive `Serialize`; remove the derive, or keep `Badge` out of the WIT")),
            "{state} {field}: {error}"
        );
        orb_with(imports(), &module(state, "", field)).unwrap_or_else(|error| panic!("{error}"));
    }
}

/// A freestanding (`no_std`) guest gets the runtime prelude once, inside its module, and a refused
/// module still brings it next to the error, so the crate reports the macro's error alone, not the
/// allocator and the panic handler it lacks without the module. A std guest never gets it.
#[test]
fn a_refused_module_brings_the_runtime_prelude_of_a_freestanding_guest() {
    let count = |tokens: &str, needle: &str| tokens.matches(needle).count();
    for (freestanding, tail) in [(true, quote::quote!(, no_std)), (false, quote::quote!())] {
        let prelude = usize::from(freestanding);
        let accepted = expansion(
            quote::quote!(package = "example:book@1.0.0" #tail),
            syn::parse_str(&ledger("#[service]")).unwrap(),
        )
        .to_string();
        assert_eq!(count(&accepted, "compile_error"), 0, "{accepted}");
        assert_eq!(
            count(&accepted, "# [global_allocator]"),
            prelude,
            "{accepted}"
        );
        assert_eq!(count(&accepted, "# [panic_handler]"), prelude, "{accepted}");
        for (attribute, module, message) in [
            (
                quote::quote!(package = "example:book@1.0.0" #tail),
                ledger("#[service]").replace("pub fn count", "pub fn then"),
                "a member every reference to a service object has",
            ),
            (
                quote::quote!(package = "example:book@1.0.0", colour #tail),
                ledger("#[service]"),
                "unknown option",
            ),
            (
                quote::quote!(package = "example:book@1.0.0" #tail),
                "pub struct NotAModule;".to_owned(),
                "expected `mod`",
            ),
        ] {
            let refused = expansion(attribute, syn::parse_str(&module).unwrap()).to_string();
            assert_eq!(count(&refused, "compile_error"), 1, "{refused}");
            assert!(refused.contains(message), "{message}\n{refused}");
            assert_eq!(
                count(&refused, "# [global_allocator]"),
                prelude,
                "{refused}"
            );
            assert_eq!(count(&refused, "# [panic_handler]"), prelude, "{refused}");
        }
    }
}

const FIRST_WIT: &str = "package example:first@1.0.0;
interface counter {
  add: func(by: u32) -> u32;
}
world first {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export counter;
}
";

/// A contract `cargo tembra build` wrote for an Orb with kernel state holds the kernel's package in
/// `deps/`: an Orb with a state of its own consuming it reads that package once, the bundled copy
/// left out of its inline WIT. The copy is the bundled file line for line, whatever its line
/// endings, as `Contract::from_dir` reads it; a copy that differs, if only in its comments, is an
/// error naming its file.
#[test]
fn a_consumed_contract_may_bring_the_kernel_package() {
    const GUEST: &str = tembra_contracts::guest::SOURCE;
    let fourth = "mod fourth {
        #[state]
        pub struct Seen { pub calls: u32 }
        #[service]
        pub struct Relay;
        impl Relay { pub fn add(by: u32) -> u32 { counter::add(by) } }
    }";
    let with_deps = |name: &str, guest: &str| {
        let dir = consumed(name, FIRST_WIT);
        std::fs::create_dir_all(dir.join("deps")).unwrap();
        std::fs::write(dir.join("deps").join("tembra-guest.wit"), guest).unwrap();
        orb_with(
            Options {
                imports: vec![literal("example:first/counter@1.0.0")],
                ..options("example:fourth@1.0.0", Some(&dir))
            },
            fourth,
        )
    };
    let checked_out = GUEST.replace('\n', "\r\n");
    for (name, guest) in [
        ("first-deps", GUEST),
        ("first-line-endings", checked_out.as_str()),
    ] {
        let module = with_deps(name, guest).unwrap_or_else(|error| panic!("{name}: {error}"));
        let generated = tokens(&module);
        assert!(
            generated.contains("inline : \"package example:fourth@1.0.0;"),
            "{generated}"
        );
        assert!(
            !generated.contains("package tembra:guest@1.0.0"),
            "{name}: {generated}"
        );
    }
    for (name, guest) in [
        (
            "first-comments",
            GUEST.replace("/// ", "/// (another text) "),
        ),
        (
            "first-different",
            GUEST.replacen("read: func() -> string;", "read: func() -> u32;", 1),
        ),
    ] {
        let error = with_deps(name, &guest).map(|_| ()).unwrap_err().to_string();
        assert!(
            error.contains("deps/tembra-guest.wit:")
                && error.contains("this copy of tembra:guest@1.0.0, the kernel's package, differs from the one bundled with this macro"),
            "{name}: {error}"
        );
    }
    // Without the package in the consumed directory, the inline WIT brings the bundled one.
    let dir = consumed("first-alone", LEDGER_WIT);
    let module = orb_with(
        options("example:fourth@1.0.0", Some(&dir)),
        &fourth.replace("counter::add(by)", "by"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(tokens(&module).contains("package tembra:guest@1.0.0 {"));
}

/// The prelude names the generated code could mean; it spells each by its path instead.
const PRELUDE: &[&str] = &[
    "Option", "Some", "None", "Result", "Ok", "Err", "Default", "From", "Into", "FnOnce", "Sync",
    "String", "Vec", "Box", "Clone", "Iterator", "format", "vec", "panic",
];

/// The prelude names a token stream uses bare, not after `::`, outside the macro's own hidden
/// modules, whose names resolve in a module of their own.
fn bare_prelude_names(stream: proc_macro2::TokenStream, found: &mut Vec<String>) {
    use proc_macro2::TokenTree;
    let mut previous: Option<TokenTree> = None;
    let mut hidden = false;
    for tree in stream {
        match &tree {
            TokenTree::Group(group) if !hidden => bare_prelude_names(group.stream(), found),
            TokenTree::Group(_) => hidden = false,
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                let after_mod =
                    matches!(&previous, Some(TokenTree::Ident(keyword)) if keyword == "mod");
                hidden = after_mod && name.starts_with("__tembra_");
                let qualified =
                    matches!(&previous, Some(TokenTree::Punct(punct)) if punct.as_char() == ':');
                if !qualified && PRELUDE.contains(&name.as_str()) {
                    found.push(name);
                }
            }
            _ => {}
        }
        previous = Some(tree);
    }
}

const SHAPES_WIT: &str = "package example:ledger@1.0.0;
interface book {
  find: func(label: string) -> option<s64>;
  check: func(amount: s64) -> result<s64, string>;
}
";

/// The generated code names the prelude's items by their paths, so a module's own `Result`,
/// `Option` or `Ok` cannot change what it means: the state's accessors and callbacks, the root
/// resources, a derived hook's conversions and an implemented interface's bridge.
#[test]
fn generated_code_names_prelude_items_by_their_paths() {
    let dir = consumed("hygiene", SHAPES_WIT);
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[state]
            pub struct Count { pub total: i64 }
            impl Count {
                #[validate]
                fn check(&self) -> core::result::Result<(), alloc::string::String> {
                    core::result::Result::Ok(())
                }
            }
            #[resource]
            pub struct Counts { pub seen: u32 }
            impl Counts {
                #[open]
                fn open() -> Self { Counts { seen: 0 } }
                #[close]
                fn close(self) {}
            }
            #[hook(composition = bail, input = Admission)]
            pub struct Guard;
            pub struct Admission { pub value: core::result::Result<u32, alloc::string::String> }
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {
                pub fn find(label: alloc::string::String) -> core::option::Option<i64> {
                    core::option::Option::None
                }
                pub fn check(amount: i64) -> core::result::Result<i64, alloc::string::String> {
                    core::result::Result::Ok(amount)
                }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let mut found = Vec::new();
    bare_prelude_names(
        quote::quote! { fn f() -> Result<(), ()> { Ok(()) } },
        &mut found,
    );
    assert_eq!(found, ["Result", "Ok"]);
    found.clear();
    bare_prelude_names(quote::quote!(#module), &mut found);
    assert!(found.is_empty(), "{found:?}\n{}", tokens(&module));
    let generated = tokens(&module);
    for expected in [
        "fn find (label : __tembra_alloc :: string :: String) -> :: core :: option :: Option < i64 >",
        "fn check (amount : i64) -> :: core :: result :: Result < i64 , __tembra_alloc :: string :: String >",
        ":: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: Ok (:: core :: convert :: Into :: into (item))",
        "-> :: core :: option :: Option < exports :: tembra :: guest :: state_init :: Declaration >",
        "pub fn update (change : impl :: core :: ops :: FnOnce (& mut Self) ,) -> :: core :: result :: Result < Self , __tembra_state :: String >",
        "if let :: core :: option :: Option :: Some (value) = __TEMBRA_RESOURCE_COUNTS . close ()",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// The native side generates `orb()`, inside the facade's wrapper: `orb_named` with the name and
/// version of the manifest's table. `orb_named` builds the Orb under the name and version it is
/// given, with one `Service` per `#[service]`, built by a hidden function of the service's module
/// and named `{orb}::{identity}`, with the signature constant and one `method_with` per `pub fn`
/// under its WIT name, whose handle checks the context first and whose codec the function
/// declares. The manifest is included, so Cargo rebuilds the crate when only the table changes.
#[test]
fn the_native_side_generates_orb_from_the_manifest_and_the_services() {
    let module = orb_as(
        options(PACKAGE, None),
        Metadata {
            name: "ledgers".into(),
            api_version: 3,
        },
        &ledger("#[service]"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let native = native(&module, &[]);
    for expected in [
        "const _ : & [u8] = :: core :: include_bytes ! (:: core :: concat ! (:: core :: env ! (\"CARGO_MANIFEST_DIR\") , \"/Cargo.toml\")) ;",
        "pub fn orb () -> :: tembra :: Result < :: tembra :: Orb > { orb_named (\"ledgers\" , 3u32) }",
        "pub fn orb_named (name : & str , api_version : u32) -> :: tembra :: Result < :: tembra :: Orb > { :: core :: result :: Result :: Ok (:: tembra :: Orb :: new (name , api_version) . service (self :: __tembra_service_ledger (name))) }",
        "pub (crate) fn __tembra_service_ledger (orb : & str) -> :: tembra :: Service < () > { const __TEMBRA_CODEC_COUNT : :: tembra :: Codec < () , u32 > = :: tembra :: Codec { decode : | value | {",
        "} ; :: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:book/ledger@1.0.0\") , | _ | { :: core :: result :: Result :: Ok (()) }) . signature (:: core :: option :: Option :: Some (:: std :: string :: String :: from (__TEMBRA_SIGNATURE_LEDGER))) . method_with (\"count\" , | _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: count ()) } , __TEMBRA_CODEC_COUNT ,) }",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    assert!(!native.contains(". lifetime ("), "{native}");
    // No type of the module enters the WIT, so there is no JSON of one to write.
    assert!(!native.contains("__TembraBoundary"), "{native}");
    // One wrapper, in the Orb module: the services are its.
    assert_eq!(tokens(&module).matches(":: tembra :: __host !").count(), 1);
}

/// The ownership policy of `#[service(lifetime = host)]` and `#[service(linger_ms = n)]` goes on
/// the `Service`, as the manifest carries it for a component; a plain service sets none.
#[test]
fn a_native_service_takes_the_lifetime_of_its_attribute() {
    let host = native(
        &expanded(PACKAGE, &ledger("#[service(lifetime = host)]")),
        &[],
    );
    assert!(
        host.contains(". lifetime (:: tembra :: core :: Lifetime :: Host , 0u32)"),
        "{host}"
    );
    let lingering = native(
        &expanded(PACKAGE, &ledger("#[service(linger_ms = 2500)]")),
        &[],
    );
    assert!(
        lingering.contains(". lifetime (:: tembra :: core :: Lifetime :: Referenced , 2500u32)"),
        "{lingering}"
    );
}

/// A method's handle takes the arguments as a tuple of the author's parameter types, in order,
/// checks the context and calls the author's function, awaited when it is `async`; a function
/// without a result answers `()`. Its codec reads the arguments from a JSON array, or `null`, of as
/// many elements as parameters, each by the kind of its WIT type: serde for a scalar, a string or
/// a char, element by element for a list, `null` or the value for an option, an array for a tuple;
/// a result is written the same way, `null` for none.
#[test]
fn a_native_method_takes_a_tuple_of_arguments_and_gets_a_codec() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[service]
            pub struct Math;
            impl Math {
                pub fn add(a: u32, b: u32) -> u32 { a + b }
                pub async fn total(values: Vec<u32>) -> u64 { 0 }
                pub fn count_words(text: String) -> Option<u32> { None }
                pub fn initial(text: Option<String>) -> Option<char> { None }
                pub fn swap(pair: (String, u32)) -> (u32, String) { (pair.1, pair.0) }
                pub fn ping() {}
            }
        }",
    );
    let native = native(&module, &[]);
    for expected in [
        ". method_with (\"add\" , | _ , __tembra_context : :: tembra :: Context , (a , b ,) : (u32 , u32 ,) | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Math :: add (a , b)) } , __TEMBRA_CODEC_ADD ,)",
        ". method_with (\"total\" , | _ , __tembra_context : :: tembra :: Context , (values ,) : (Vec < u32 > ,) | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Math :: total (values) . await) } , __TEMBRA_CODEC_TOTAL ,)",
        ". method_with (\"count-words\" ,",
        ". method_with (\"ping\" , | _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; Math :: ping () ; :: core :: result :: Result :: Ok (()) } , __TEMBRA_CODEC_PING ,)",
        "const __TEMBRA_CODEC_ADD : :: tembra :: Codec < (u32 , u32 ,) , u32 > = :: tembra :: Codec {",
        ":: tembra :: serde_json :: Value :: Null => :: std :: vec :: Vec :: new () ,",
        "let [__tembra_0 , __tembra_1] = < [:: tembra :: serde_json :: Value ; 2usize] > :: try_from (items) . map_err (| items | :: std :: format ! (\"expected 2 arguments, got {}\" , items . len ())) ? ;",
        ":: tembra :: serde_json :: from_value :: < u32 > (__tembra_0) . map_err (| error | error . to_string ())",
        ". collect :: < :: core :: result :: Result < Vec < u32 > , :: std :: string :: String >> ()",
        "const __TEMBRA_CODEC_COUNT_WORDS : :: tembra :: Codec < (String ,) , Option < u32 > >",
        ":: core :: option :: Option :: None => { :: core :: result :: Result :: Ok (:: tembra :: serde_json :: Value :: Null) } :: core :: option :: Option :: Some (item) => :: tembra :: serde_json :: to_value (item) . map_err (| error | error . to_string ()) ,",
        "const __TEMBRA_CODEC_INITIAL : :: tembra :: Codec < (Option < String > ,) , Option < char > >",
        "match __tembra_0 { :: tembra :: serde_json :: Value :: Null => { :: core :: result :: Result :: Ok (:: core :: option :: Option :: None) } item => :: tembra :: serde_json :: from_value :: < String > (item) . map_err (| error | error . to_string ()) . map (:: core :: option :: Option :: Some) , } ?",
        "const __TEMBRA_CODEC_SWAP : :: tembra :: Codec < ((String , u32) ,) , (u32 , String) >",
        "\"expected a tuple of 2 elements, got {}\"",
        "const __TEMBRA_CODEC_PING : :: tembra :: Codec < () , () > = :: tembra :: Codec { decode : | value | {",
        "encode : | () | :: core :: result :: Result :: Ok (:: tembra :: serde_json :: Value :: Null) , } ;",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
}

/// A service of a sub-module is built by a function of its module, where its struct and its types
/// resolve as written, whatever the module's and the struct's visibility; `orb()` reaches it by
/// path, and it reaches the signature constant of the Orb module through `super::`.
#[test]
fn a_native_service_of_a_sub_module_is_built_in_its_module() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub mod pinned {
                #[service(lifetime = host)]
                pub struct Pinned;
                impl Pinned { pub fn add(amount: i64) -> i64 { amount } }
            }
            mod memo {
                #[service]
                pub struct Memo;
                impl Memo { pub fn echo(text: String) -> String { text } }
            }
        }",
    );
    let root = native(&module, &[]);
    assert!(
        root.contains(
            ". service (self :: pinned :: __tembra_service_pinned (name)) . service (self :: memo :: __tembra_service_memo (name))"
        ),
        "{root}"
    );
    assert!(!root.contains("fn __tembra_service_"), "{root}");
    let pinned = native(&module, &["pinned"]);
    for expected in [
        "pub (crate) fn __tembra_service_pinned (orb : & str) -> :: tembra :: Service < () >",
        ":: std :: string :: String :: from (super :: __TEMBRA_SIGNATURE_PINNED)",
        ". lifetime (:: tembra :: core :: Lifetime :: Host , 0u32)",
        ":: core :: result :: Result :: Ok (Pinned :: add (amount))",
        "const __TEMBRA_CODEC_ADD : :: tembra :: Codec < (i64 ,) , i64 >",
    ] {
        assert!(pinned.contains(expected), "{expected}\n{pinned}");
    }
    assert!(
        native(&module, &["memo"]).contains("pub (crate) fn __tembra_service_memo (orb : & str)"),
        "{}",
        native(&module, &["memo"])
    );
}

/// Each service gets a typed client in its module, `MathClient` for `Math`, as visible as the
/// service's struct: `new` wraps a reference to one of its objects, and each function of the
/// interface is an async method, with the function's documentation, that takes the call's
/// `&Context` first and then the function's parameters, and calls the method locally with them as
/// a tuple in WIT order, `()` without parameters, for its result, `()` without one. A function
/// named `new` is `new_` on the client, and keeps its WIT name.
#[test]
fn a_native_service_gets_a_typed_client() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[service]
            pub struct Math;
            impl Math {
                /// The sum of two numbers.
                pub fn add(a: u32, b: u32) -> u32 { a + b }
                pub async fn total(values: Vec<u32>) -> u64 { 0 }
                pub fn count_words(text: String) -> Option<u32> { None }
                pub fn ping() {}
                pub fn new() -> u32 { 0 }
            }
            pub mod memo {
                #[service]
                pub(crate) struct Memo;
                impl Memo { pub fn echo(context: String) -> String { context } }
            }
        }",
    );
    let root = native(&module, &[]);
    for expected in [
        "pub struct MathClient { pub (crate) reference : :: std :: rc :: Rc < :: tembra :: Reference > , }",
        "impl MathClient {",
        "pub fn new (reference : :: std :: rc :: Rc < :: tembra :: Reference >) -> Self { Self { reference } }",
        "# [doc = \" The sum of two numbers.\"] pub async fn add (& self , context : & :: tembra :: Context , a : u32 , b : u32) -> :: tembra :: Result < u32 > { self . reference . call :: < (u32 , u32 ,) , u32 > (context , \"add\" , (a , b ,)) . await }",
        "pub async fn total (& self , context : & :: tembra :: Context , values : Vec < u32 >) -> :: tembra :: Result < u64 > { self . reference . call :: < (Vec < u32 > ,) , u64 > (context , \"total\" , (values ,)) . await }",
        "pub async fn count_words (& self , context : & :: tembra :: Context , text : String) -> :: tembra :: Result < Option < u32 > > { self . reference . call :: < (String ,) , Option < u32 > > (context , \"count-words\" , (text ,)) . await }",
        "pub async fn ping (& self , context : & :: tembra :: Context) -> :: tembra :: Result < () > { self . reference . call :: < () , () > (context , \"ping\" , ()) . await }",
        "pub async fn new_ (& self , context : & :: tembra :: Context) -> :: tembra :: Result < u32 > { self . reference . call :: < () , u32 > (context , \"new\" , ()) . await }",
        ". method_with (\"new\" ,",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    assert!(!root.contains("pub async fn new ("), "{root}");
    assert!(!root.contains("MemoClient"), "{root}");
    let memo = native(&module, &["memo"]);
    for expected in [
        "pub (crate) struct MemoClient {",
        "pub async fn echo (& self , context : & :: tembra :: Context , context : String) -> :: tembra :: Result < String > { self . reference . call :: < (String ,) , String > (context , \"echo\" , (context ,)) . await }",
    ] {
        assert!(memo.contains(expected), "{expected}\n{memo}");
    }
}

const COUNTER_WIT: &str = "package example:counters@1.0.0;
interface counter {
  add: func(amount: u32) -> u32;
}
world counting { export counter; }
";

/// A service implementing a consumed interface is registered under that interface's identity, with
/// the signature its own contract gives it.
#[test]
fn a_native_service_implementing_an_interface_is_named_by_its_identity() {
    let dir = consumed("native-implemented", COUNTER_WIT);
    let module = orb_with(
        options("example:tally@1.0.0", Some(&dir)),
        "mod tally {
            #[service(\"example:counters/counter@1.0.0\")]
            pub struct Counter;
            impl Counter { pub fn add(amount: u32) -> u32 { amount } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let native = native(&module, &[]);
    for expected in [
        ":: std :: format ! (\"{}::{}\" , orb , \"example:counters/counter@1.0.0\")",
        ":: std :: string :: String :: from (__TEMBRA_SIGNATURE_COUNTER)",
        ". method_with (\"add\" ,",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
}

/// A method may take the call's `&Context`, anywhere among its parameters and through any path to
/// `Context`: the WIT, the arguments' tuple and the codec leave it out, and the native handle
/// passes its own context there. A component's bridge, derived or implemented, never calls such a
/// method; it reads its parameters, so the host compiles it cleanly. A second `&Context` is
/// refused.
#[test]
fn a_native_method_takes_the_calls_context_outside_the_wit() {
    let module = expanded(
        PACKAGE,
        "mod book {
            use tembra::Context;
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn owner(context: &Context) -> String { String::new() }
                pub async fn add(amount: u32, context: &::tembra::Context, label: String) -> u32 { amount }
            }
        }",
    );
    let wit = wit(&module);
    for expected in [
        "owner: func() -> string;",
        "add: async func(amount: u32, label: string) -> u32;",
    ] {
        assert!(wit.contains(expected), "{expected}\n{wit}");
    }
    assert!(!wit.contains("context"), "{wit}");
    let handles = native(&module, &[]);
    for expected in [
        ". method_with (\"owner\" , | _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: owner (& __tembra_context)) } , __TEMBRA_CODEC_OWNER ,)",
        ". method_with (\"add\" , | _ , __tembra_context : :: tembra :: Context , (amount , label ,) : (u32 , String ,) | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: add (amount , & __tembra_context , label) . await) } , __TEMBRA_CODEC_ADD ,)",
        "const __TEMBRA_CODEC_OWNER : :: tembra :: Codec < () , String >",
        "const __TEMBRA_CODEC_ADD : :: tembra :: Codec < (u32 , String ,) , u32 >",
    ] {
        assert!(handles.contains(expected), "{expected}\n{handles}");
    }
    let generated = tokens(&module);
    for expected in [
        "fn owner () -> String { :: core :: unreachable ! (\"`Ledger::owner` takes the call's `&Context`, which only a native host passes\") }",
        "async fn add (amount : u32 , label : String) -> u32 { let _ = amount ; let _ = label ; :: core :: unreachable ! (\"`Ledger::add` takes the call's `&Context`, which only a native host passes\") }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let dir = consumed("native-context", COUNTER_WIT);
    let module = orb_with(
        options("example:tally@1.0.0", Some(&dir)),
        "mod tally {
            #[service(\"example:counters/counter@1.0.0\")]
            pub struct Counter;
            impl Counter { pub fn add(context: &tembra::Context, amount: u32) -> u32 { amount } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let implemented = native(&module, &[]);
    assert!(
        implemented.contains(
            "(amount ,) : (u32 ,) | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Counter :: add (& __tembra_context , amount)) }"
        ),
        "{implemented}"
    );
    let generated = tokens(&module);
    assert!(
        generated.contains(
            "fn add (amount : u32) -> u32 { let _ = amount ; :: core :: unreachable ! (\"`Counter::add` takes the call's `&Context`, which only a native host passes\") }"
        ),
        "{generated}"
    );
    assert_eq!(
        refused(
            PACKAGE,
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn twice(first: &Context, second: &Context) {} }
            }"
        ),
        "a method takes the call's `&Context` once"
    );
    // A module with a `Context` of its own, a record or a struct it keeps, means that one.
    for declared in [
        "pub struct Context { pub user: String }",
        "pub struct Context(String);",
    ] {
        assert_eq!(
            refused(
                PACKAGE,
                &format!(
                    "mod book {{
                        {declared}
                        #[service]
                        pub struct Ledger;
                        impl Ledger {{ pub fn greet(c: &Context) -> String {{ String::new() }} }}
                    }}"
                )
            ),
            "`Ledger::greet` takes `&Context`, and this module declares a `Context` of its own: pass that type owned, as WIT parameters are, or rename it for the method to take the call's `&Context`",
            "{declared}"
        );
    }
}

/// A `pub fn` marked `#[local]` is a method for native callers only: the WIT leaves it out, with
/// the types only it reaches, which need no WIT spelling and stay the author's; the native side
/// registers it with `local_method`, under the name it would have in WIT and with the handle of
/// any method, and writes no codec for it; the component bridges no such function. The marker
/// goes on a `pub fn` of a service, whose other methods, local or portable, take other names, and
/// which keeps a portable one.
#[test]
fn a_local_method_is_registered_natively_and_kept_out_of_the_wit() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub struct Tape { pub entries: Vec<u32> }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn count() -> u32 { 0 }
                /// Run a callback over a tape.
                #[local]
                pub fn apply_each(tape: Tape, operation: Rc<dyn Fn(u32) -> u32>) -> Tape { tape }
                #[local]
                pub async fn rewind(context: &Context, label: &'static str) {}
            }
        }",
    );
    let wit = wit(&module);
    assert!(wit.contains("count: func() -> u32;"), "{wit}");
    for absent in ["apply", "callback", "rewind", "tape"] {
        assert!(!wit.contains(absent), "{absent}\n{wit}");
    }
    let handles = native(&module, &[]);
    for expected in [
        ". method_with (\"count\" ,",
        ". local_method (\"apply-each\" , | _ , __tembra_context : :: tembra :: Context , (tape , operation ,) : (Tape , Rc < dyn Fn (u32) -> u32 > ,) | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: apply_each (tape , operation)) } ,)",
        ". local_method (\"rewind\" , | _ , __tembra_context : :: tembra :: Context , (label ,) : (& 'static str ,) | async move { __tembra_context . check () ? ; Ledger :: rewind (& __tembra_context , label) . await ; :: core :: result :: Result :: Ok (()) } ,)",
    ] {
        assert!(handles.contains(expected), "{expected}\n{handles}");
    }
    for absent in ["__TEMBRA_CODEC_APPLY_EACH", "__TEMBRA_CODEC_REWIND"] {
        assert!(!handles.contains(absent), "{absent}\n{handles}");
    }
    // The client calls a local method as any other, with its documentation.
    for expected in [
        "# [doc = \" Run a callback over a tape.\"] pub async fn apply_each (& self , context : & :: tembra :: Context , tape : Tape , operation : Rc < dyn Fn (u32) -> u32 >) -> :: tembra :: Result < Tape >",
        ". call :: < (Tape , Rc < dyn Fn (u32) -> u32 > ,) , Tape > (context , \"apply-each\" , (tape , operation ,))",
        "pub async fn rewind (& self , context : & :: tembra :: Context , label : & 'static str) -> :: tembra :: Result < () >",
    ] {
        assert!(handles.contains(expected), "{expected}\n{handles}");
    }
    let generated = tokens(&module);
    for expected in [
        "pub struct Tape { pub entries : Vec < u32 > }",
        "Guest for __TembraExports { fn count () -> u32 { Ledger :: count () } }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    for (module, message) in [
        (
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger {
                    pub fn count() -> u32 { 0 }
                    #[local]
                    fn hidden() {}
                }
            }",
            "#[local] marks a `pub fn` of a #[service] struct",
        ),
        (
            "mod book {
                pub struct Tape;
                impl Tape { #[local] pub fn rewind() {} }
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn count() -> u32 { 0 } }
            }",
            "#[local] marks a function of a #[service] struct; `Tape` is not one",
        ),
        (
            "mod book {
                #[local]
                pub fn rewind() {}
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn count() -> u32 { 0 } }
            }",
            "#[local] goes on a function in the impl block of a #[service] struct, not on a free function",
        ),
        // A local method's name is taken among the portable methods' and the other local ones'.
        (
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger {
                    pub fn get_entry(id: u32) -> String { String::new() }
                    #[local]
                    pub fn getEntry(id: u32, cache: Rc<Cache>) -> String { String::new() }
                }
            }",
            "`Ledger::get_entry` and `Ledger::getEntry` are both `get-entry` in the methods of `Ledger`, where local and portable methods share one namespace; rename one of them",
        ),
        (
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger {
                    pub fn count() -> u32 { 0 }
                    #[local]
                    pub fn rewind_tape() {}
                    #[local]
                    pub fn rewindTape() {}
                }
            }",
            "`Ledger::rewind_tape` and `Ledger::rewindTape` are both `rewind-tape` in the methods of `Ledger`, where local and portable methods share one namespace; rename one of them",
        ),
        // Its interface is made of the portable ones.
        (
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger { #[local] pub fn rewind() {} }
            }",
            "a service needs at least one portable method, a pub fn without #[local], or a function marked #[create] that declares what its objects run",
        ),
        // It borrows `&self` only where a create builds the object, as a portable one does.
        (
            "mod book {
                #[service]
                pub struct Ledger;
                impl Ledger {
                    pub fn count() -> u32 { 0 }
                    #[local]
                    pub fn rewind(&self) {}
                }
            }",
            "`Ledger::rewind` borrows `&self`, which a #[create] builds: give `Ledger` a function marked #[create], or remove self",
        ),
    ] {
        assert_eq!(refused(PACKAGE, module), message);
    }
}

/// With `context`, a native host gets a native `context` in the Orb module, in place of the
/// component's: the kernel interface's types and its functions, which read the call whose method
/// is being polled. Each method's future, local ones too, runs in the scope of its call, which a
/// sub-module's service reaches through `super::`. A refused module keeps its `context`, so the
/// author's calls to it add no errors to the refusal.
#[test]
fn the_native_context_reads_the_call_each_method_runs_for() {
    let module = orb_with(
        Options {
            context: true,
            ..options(PACKAGE, None)
        },
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn count() -> u32 { 0 }
                #[local]
                pub fn rewind() {}
            }
            pub mod shelf {
                #[service]
                pub struct Shelf;
                impl Shelf { pub fn size() -> u32 { 0 } }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let root = native(&module, &[]);
    for expected in [
        "pub mod context { pub use super :: tembra :: guest :: context :: { Address , Level } ;",
        "pub (crate) fn __scoped < F : :: core :: future :: Future > (context : :: tembra :: Context , future : F ,) -> impl :: core :: future :: Future < Output = F :: Output > {",
        "let principal = context . principal () . unwrap_or_default () ;",
        "pub fn object () -> Address {",
        "pub fn principal () -> :: std :: string :: String { current () . principal . clone () }",
        "pub fn cancelled () -> bool { current () . context . signal () . is_cancelled () }",
        "pub fn cancel () {",
        "pub fn log (level : Level , message : & str) {",
        ". method_with (\"count\" , | _ , __tembra_context : :: tembra :: Context , () : () | self :: context :: __scoped (__tembra_context . clone () , async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: count ()) }) , __TEMBRA_CODEC_COUNT ,)",
        ". local_method (\"rewind\" , | _ , __tembra_context : :: tembra :: Context , () : () | self :: context :: __scoped (__tembra_context . clone () , async move { __tembra_context . check () ? ; Ledger :: rewind () ; :: core :: result :: Result :: Ok (()) }) ,)",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let shelf = native(&module, &["shelf"]);
    assert!(
        shelf.contains("| self :: super :: context :: __scoped (__tembra_context . clone () ,"),
        "{shelf}"
    );
    assert!(!shelf.contains("pub mod context"), "{shelf}");
    let refused = orb_with(
        Options {
            context: true,
            imports: vec![literal("wasi:random/random@0.3.0")],
            ..options(PACKAGE, None)
        },
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let refused = native(&refused, &[]);
    assert!(
        refused.starts_with(
            ":: core :: compile_error ! (\"the import of wasi:random/random@0.3.0 is not supported natively yet\") ;"
        ),
        "{refused}"
    );
    assert!(refused.contains("pub mod context {"), "{refused}");
}

/// A `#[wit_resource]` struct is `Issuable` as the resource of its service's interface, and its
/// functions are portable methods its module adds to the service: the constructor issues the value
/// it builds into the call's result, whose data is its index there, a method reads its `self` from
/// the call through a `Receiver`, and a static function or a service function whose result holds
/// the resource issues what it returns, also inside a container. Such a function has no local
/// entrypoint, so the service's client calls it portably.
#[test]
fn a_wit_resource_is_issued_natively_and_read_as_self() {
    let module = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn many(n: u32) -> Vec<Session> { Vec::new() }
            pub fn count() -> u32 { 0 }",
        ),
    );
    let native = native(&module, &[]);
    let issued = "< u32 as :: core :: convert :: From < :: tembra :: portable :: Slot >> :: from (__tembra_result . issue";
    let written = "-> :: core :: result :: Result < :: tembra :: serde_json :: Value , :: std :: string :: String >";
    let issuing = format!("__tembra_result : & mut :: tembra :: portable :: Payload | {written}");
    for expected in [
        "impl :: tembra :: portable :: Issuable for Session { fn nominal () -> :: tembra :: portable :: Nominal { :: tembra :: portable :: Nominal { interface : :: std :: string :: String :: from (\"example:desk/sessions@1.0.0\") , name : :: std :: string :: String :: from (\"session\") , } } }",
        "pub (crate) fn __tembra_service_sessions (orb : & str) -> :: tembra :: Service < () > { const __TEMBRA_CODEC_COUNT",
        "__tembra_resource_session (:: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:desk/sessions@1.0.0\")",
        "pub (crate) fn __tembra_resource_session < E : 'static > (service : :: tembra :: Service < E >) -> :: tembra :: Service < E > { service . payload_method (\"[constructor]session\" ,",
        &format!(
            "let (n ,) : (u32 ,) = decode (__tembra_payload . into_data () ?) . map_err (| message | {{ :: tembra :: Error :: invalid (:: tembra :: core :: Reason :: Arguments , message) }}) ? ; let value = Session :: new (n) ; let encode = | value : Session , {issuing}"
        ),
        &format!(
            "{issued} (value)))) }} ; let mut __tembra_result = :: tembra :: portable :: Payload :: data (:: tembra :: serde_json :: Value :: Null ,) ; let written = encode (value , & mut __tembra_result) . map_err (| message | :: tembra :: Error :: invalid (:: tembra :: core :: Reason :: Result , message)) ? ; __tembra_result . map_value (| _ | :: core :: result :: Result :: Ok (written))"
        ),
        ". payload_method (\"[method]session.get\" , | _ , __tembra_context : :: tembra :: Context , mut __tembra_payload : :: tembra :: portable :: Payload | async move { __tembra_context . check () ? ; let __tembra_receiver = __tembra_payload . receiver :: < Session > () ? ;",
        &format!(
            "let value = __tembra_receiver . with (| __tembra_value | Session :: get (__tembra_value ,)) ? ; let encode = | value : u32 | {written} {{ :: tembra :: serde_json :: to_value (value)"
        ),
        &format!(
            "let value = __tembra_receiver . with (| __tembra_value | Session :: twin (__tembra_value ,)) ? ; let encode = | value : Session , {issuing}"
        ),
        ". payload_method (\"[static]session.make\" ,",
        &format!("let value = Session :: make () ; let encode = | value : Session , {issuing}"),
        ". payload_method (\"open\" , | _ , __tembra_context : :: tembra :: Context , __tembra_payload : :: tembra :: portable :: Payload | async move {",
        &format!("let value = Sessions :: open (n) ; let encode = | value : Session , {issuing}"),
        &format!(
            "let value = Sessions :: many (n) ; let encode = | value : Vec < Session > , {issuing}"
        ),
        &format!(
            "value . into_iter () . map (| item | :: core :: result :: Result :: < :: tembra :: serde_json :: Value , :: std :: string :: String > :: Ok (:: tembra :: serde_json :: Value :: from ({issued} (item)))))"
        ),
        ". method_with (\"count\" ,",
        "pub async fn count (& self , context : & :: tembra :: Context) -> :: tembra :: Result < u32 >",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    // A function handing out a resource has no local entrypoint: its client calls it portably.
    for absent in [". method_with (\"open\"", "\"open\" , (n ,)"] {
        assert!(!native.contains(absent), "{absent}\n{native}");
    }
    // With the `context` option, the resource's functions and a function handing it out run in
    // the scope of their call, as a service's methods do.
    let module = orb_with(
        Options {
            context: true,
            ..options("example:desk@1.0.0", None)
        },
        DESK,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let in_context = self::native(&module, &[]);
    let scoped = "| self :: context :: __scoped (__tembra_context . clone () , async move { __tembra_context . check () ? ;";
    for function in ["open", "[constructor]session", "[method]session.get"] {
        let receiver = if function.starts_with("[method]") {
            "mut __tembra_payload"
        } else {
            "__tembra_payload"
        };
        let expected = format!(
            ". payload_method (\"{function}\" , | _ , __tembra_context : :: tembra :: Context , {receiver} : :: tembra :: portable :: Payload {scoped}"
        );
        assert!(in_context.contains(&expected), "{expected}\n{in_context}");
    }
}

/// A native caller's typed calls of a WIT resource: the resource's handle type, `SessionHandle`,
/// owns an `Owned` handle and has one async method per method of the resource, which takes the
/// call's context first and calls the method through the handle; the service's client gets the
/// constructor, `new_session`, and the static functions, `session_make`, and calls a function
/// handing the resource out portably. Each answers a bare resource as its handle, from the `Owned`
/// handle the call delivers, and nested ones from the `Delivered` result, each handle at the index
/// the value names.
#[test]
fn a_wit_resource_gets_a_typed_handle_and_client_calls() {
    let module = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn many(n: u32) -> (Vec<Session>, u32) { (Vec::new(), n) }",
        ),
    );
    let native = native(&module, &[]);
    let arguments = "let __tembra_arguments = __tembra_encode () . map_err (| message | { :: tembra :: Error :: invalid (:: tembra :: core :: Reason :: Arguments , message) }) ? ;";
    let wrapped = "< SessionHandle as :: core :: convert :: TryFrom < :: tembra :: portable :: Owned >> :: try_from";
    for expected in [
        "pub struct SessionHandle { owned : :: tembra :: portable :: Owned , }",
        "# [doc = \" The number.\"] pub async fn get (& self , context : & :: tembra :: Context) -> :: tembra :: Result < u32 > {",
        arguments,
        "let __tembra_value = self . owned . call_portable :: < _ , :: tembra :: serde_json :: Value > (context , \"[method]session.get\" , __tembra_arguments) . await ? ;",
        "pub async fn twin (& self , context : & :: tembra :: Context) -> :: tembra :: Result < SessionHandle > {",
        &format!(
            "{wrapped} (self . owned . call_portable :: < _ , :: tembra :: portable :: Owned > (context , \"[method]session.twin\" , __tembra_arguments) . await ?)"
        ),
        // A handle of another resource type comes back as the error, still open.
        "impl :: core :: convert :: TryFrom < :: tembra :: portable :: Owned > for SessionHandle { type Error = :: tembra :: portable :: Owned ;",
        "if * owned . nominal () != < Session as :: tembra :: portable :: Issuable > :: nominal () { return :: core :: result :: Result :: Err (owned) ; }",
        "impl :: core :: convert :: From < SessionHandle > for :: tembra :: portable :: Owned {",
        "impl SessionsClient { # [doc = \" A new `Session`, from its constructor, as its handle.\"] pub async fn new_session (& self , context : & :: tembra :: Context , n : u32) -> :: tembra :: Result < SessionHandle > {",
        ":: tembra :: serde_json :: Value :: Array (:: std :: vec ! [:: tembra :: serde_json :: to_value (n) . map_err (| error | error . to_string ()) ?] ,)",
        &format!(
            "{wrapped} (self . reference . call_portable :: < _ , :: tembra :: portable :: Owned > (context , \"[constructor]session\" , __tembra_arguments) . await ?)"
        ),
        "pub async fn session_make (& self , context : & :: tembra :: Context) -> :: tembra :: Result < SessionHandle > {",
        "pub async fn open (& self , context : & :: tembra :: Context , n : u32) -> :: tembra :: Result < SessionHandle > {",
        "pub async fn many (& self , context : & :: tembra :: Context , n : u32) -> :: tembra :: Result < (:: std :: vec :: Vec < SessionHandle > , u32 ,) > {",
        "let __tembra_delivered = self . reference . call_portable :: < _ , :: tembra :: portable :: Delivered > (context , \"many\" , __tembra_arguments) . await ? ;",
        ". and_then (| index : usize | __tembra_owned . get_mut (index)) . and_then (:: core :: option :: Option :: take)",
        ". collect :: < :: core :: result :: Result < :: std :: vec :: Vec < _ > , :: std :: string :: String >> ()",
        "if __tembra_owned . iter () . any (:: core :: option :: Option :: is_some) {",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    // A resource in a sub-module of its service's module: its handle sits there, and so do the
    // client's calls of its constructor, on the client of the service's module.
    let module = expanded(
        "example:desk@1.0.0",
        "mod desk {
            #[service]
            pub struct Sessions;
            impl Sessions {
                pub fn open(n: u32) -> kinds::Session { kinds::Session::new(n) }
            }
            pub mod kinds {
                #[wit_resource(service = Sessions)]
                pub struct Session { n: u32 }
                impl Session {
                    #[constructor]
                    pub fn new(n: u32) -> Session { Session { n } }
                    pub fn get(&self) -> u32 { self.n }
                }
            }
        }",
    );
    let root = self::native(&module, &[]);
    assert!(
        root.contains("pub async fn open (& self , context : & :: tembra :: Context , n : u32) -> :: tembra :: Result < kinds :: SessionHandle > {"),
        "{root}"
    );
    let kinds = self::native(&module, &["kinds"]);
    for expected in [
        "pub struct SessionHandle {",
        "impl super :: SessionsClient { # [doc = \" A new `Session`, from its constructor, as its handle.\"] pub async fn new_session (& self , context : & :: tembra :: Context , n : u32) -> :: tembra :: Result < super :: kinds :: SessionHandle > {",
    ] {
        assert!(kinds.contains(expected), "{expected}\n{kinds}");
    }
}

/// The client of a service names each function of its interface, and the constructor and static
/// functions of each of its resources, `new_<resource>` and `<resource>_<function>`: two methods of
/// one name, which the compiler would refuse as one method defined twice, are refused natively,
/// naming both, a function of the service and a resource's, or the functions of two resources. A
/// component build, which has no client, takes them.
#[test]
fn the_native_client_refuses_two_methods_of_one_name() {
    // The constructor of `Kind` and the static function `kind` of `New`.
    let two = "mod desk {
        #[wit_resource]
        pub struct Kind { n: u32 }
        impl Kind {
            #[constructor]
            pub fn new(n: u32) -> Kind { Kind { n } }
            pub fn get(&self) -> u32 { self.n }
        }
        #[wit_resource]
        pub struct New { n: u32 }
        impl New {
            pub fn kind() -> u32 { 0 }
            pub fn get(&self) -> u32 { self.n }
        }
        #[service]
        pub struct Sessions;
        impl Sessions {
            pub fn open(n: u32) -> New { New { n } }
        }
    }";
    for (module, message) in [
        (
            DESK.replace(
                "pub fn open(n: u32) -> Session { Session::new(n) }",
                "pub fn open(n: u32) -> Session { Session::new(n) }
                pub fn new_session(n: u32) -> u32 { n }",
            ),
            "the client of `Sessions` would name both the function `Sessions::new_session` and the constructor of `Session` `new_session`; rename one of them",
        ),
        (
            DESK.replace(
                "pub fn open(n: u32) -> Session { Session::new(n) }",
                "pub fn open(n: u32) -> Session { Session::new(n) }
                pub fn session_make() -> u32 { 0 }",
            ),
            "the client of `Sessions` would name both the function `Sessions::session_make` and the static function `Session::make` `session_make`; rename one of them",
        ),
        (
            two.to_owned(),
            "the client of `Sessions` would name both the constructor of `Kind` and the static function `New::kind` `new_kind`; rename one of them",
        ),
    ] {
        let module = orb_with(options("example:desk@1.0.0", None), &module)
            .unwrap_or_else(|error| panic!("{message}: {error}"));
        assert_eq!(
            native(&module, &[]),
            format!(":: core :: compile_error ! (\"{message}\") ;"),
            "{message}"
        );
        assert!(wit(&module).starts_with("package "), "{message}");
    }
}

/// A native-only service whose create builds its objects may have no method of its own; a WIT
/// resource of its interface still makes that interface one a contract plans, so the service has
/// a signature, as one with methods does.
#[test]
fn a_service_with_only_a_resource_has_a_signature() {
    let module = orb_with(
        options("example:desk@1.0.0", None),
        "mod desk {
            use tembra::Declaration;
            #[inputs]
            pub struct Inputs { pub n: u32 }
            #[wit_resource(service = Sessions)]
            pub struct Session { n: u32 }
            impl Session {
                #[constructor]
                pub fn new(n: u32) -> Session { Session { n } }
                pub fn get(&self) -> u32 { self.n }
            }
            #[service]
            pub struct Sessions { n: u32 }
            impl Sessions {
                #[create]
                fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                    Ok(Self { n: inputs.n })
                }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let generated = tokens(&module);
    assert!(
        generated.contains("pub const __TEMBRA_SIGNATURE_SESSIONS : & str ="),
        "{generated}"
    );
    let native = native(&module, &[]);
    assert!(
        native.contains("__tembra_resource_session (:: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:desk/sessions@1.0.0\") , move | declaration | Sessions :: create (& inputs , & declaration)) . signature (:: core :: option :: Option :: Some (:: std :: string :: String :: from (__TEMBRA_SIGNATURE_SESSIONS))))"),
        "{native}"
    );
}

/// Natively a resource is only returned, or borrowed as a method's `self`: a function of a service
/// or of a resource taking one, itself or inside a container, is refused, and so is a record or a
/// variant holding one, which the bindings replace with a type holding their handle, which only a
/// component creates, so it is component-only and the refusal points to a tuple.
#[test]
fn the_native_side_refuses_a_resource_it_cannot_carry() {
    let taken =
        "a resource; natively a resource is only returned, or borrowed as a method's `self`";
    for (from, to, message) in [
        (
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn close(session: Session) -> u32 { session.n }",
            format!("`Sessions::close` takes `Session`, {taken}"),
        ),
        (
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn close_all(sessions: Vec<Session>) {}",
            format!("`Sessions::close_all` takes `Vec<Session>`, which holds `Session`, {taken}"),
        ),
        (
            "pub fn get(&self) -> u32 { self.n }",
            "pub fn get(&self) -> u32 { self.n }
            pub fn merge(&self, other: Session) -> u32 { self.n + other.n }",
            format!("`Session::merge` takes `Session`, {taken}"),
        ),
        (
            "pub fn new(n: u32) -> Session { Session { n } }",
            "pub fn new(n: u32, from: Option<Session>) -> Session { Session { n } }",
            format!("`Session::new` takes `Option<Session>`, which holds `Session`, {taken}"),
        ),
        (
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) }
            pub fn lend(n: u32) -> Lease { todo!() }
        }
        pub struct Lease { pub session: Session, pub count: u32 }
        impl Sessions {",
            "the field `session` of `Lease` is `Session`, a resource; a record or a variant holding a resource is component-only, since the bindings replace it with their type, whose handle only a component creates: to return the resource natively too, return it in a tuple beside the data, such as `-> (Session, String)`".to_owned(),
        ),
    ] {
        let module = orb_with(options("example:desk@1.0.0", None), &DESK.replace(from, to))
            .unwrap_or_else(|error| panic!("{message}: {error}"));
        assert_eq!(
            native(&module, &[]),
            format!(":: core :: compile_error ! (\"{message}\") ;"),
            "{message}"
        );
        assert!(wit(&module).starts_with("package "), "{message}");
    }
}

/// An `async` method of a WIT resource borrows its `self` natively from the guard its call's
/// receiver gives, held for the whole of the method's future, so the value stays the method's
/// across its awaits; the WIT keeps the method `async`.
#[test]
fn an_async_method_of_a_wit_resource_holds_its_self_natively() {
    let module = expanded(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn get(&self) -> u32 { self.n }",
            "pub fn get(&self) -> u32 { self.n }
            pub async fn later(&self, by: u32) -> u32 { self.n + by }",
        ),
    );
    assert!(
        wit(&module).contains("    later: async func(by: u32) -> u32;\n"),
        "{}",
        wit(&module)
    );
    let native = native(&module, &[]);
    for expected in [
        ". payload_method (\"[method]session.later\" , | _ , __tembra_context : :: tembra :: Context , mut __tembra_payload : :: tembra :: portable :: Payload | async move { __tembra_context . check () ? ; let __tembra_receiver = __tembra_payload . receiver :: < Session > () ? ;",
        "let value = { let __tembra_self = __tembra_receiver . guard () ? ; Session :: later (& * __tembra_self , by) . await } ;",
        // A synchronous method still reads its `self` only while it runs.
        "let value = __tembra_receiver . with (| __tembra_value | Session :: get (__tembra_value ,)) ? ;",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
}

/// A state natively: the factory of the service that owns it declares it, with the version and the
/// rule of the state, before the create runs, and keeps it beside the object's environment, a unit
/// or what the create builds; each method's future, local ones too, runs in the scope of its
/// object, where the state's `read` and `update` reach it, in a sub-module through `super::`, and
/// so do the futures of a function issuing a WIT resource of the service and of that resource's
/// functions, which the service's registration hands the way to the object's state. The
/// component's `read` and `update`, over the bindings' import, are a component build's only, and a
/// native host gets its own in the same module, which the state's accessors call in both. A service
/// that owns no state is registered as without one.
#[test]
fn the_factory_of_its_service_declares_a_state_natively() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[inputs]
            pub struct Inputs { pub title: String }
            #[state(service = Notes, version = 2)]
            pub struct Kept { pub count: u32 }
            impl Kept {
                #[validate]
                fn bounded(&self) -> Result<(), String> { Ok(()) }
            }
            #[state(service = Ledger)]
            pub struct Total { pub sum: u32 }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn count() -> u32 { Total::read().sum }
                pub fn issue() -> Ticket { Ticket }
            }
            #[wit_resource(service = Ledger)]
            pub struct Ticket;
            impl Ticket {
                #[constructor]
                pub fn new() -> Self { Ticket }
                pub fn sum(&self) -> u32 { Total::read().sum }
            }
            #[service]
            pub struct Shelf;
            impl Shelf { pub fn size() -> u32 { 0 } }
            pub mod notes {
                #[service]
                pub struct Notes { title: String }
                impl Notes {
                    #[create]
                    fn create(inputs: &super::Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                        Ok(Self { title: inputs.title.clone() })
                    }
                    pub fn title(&self) -> String { self.title.clone() }
                    #[local]
                    pub fn kept() -> u32 { super::Kept::read().count }
                }
            }
        }",
    );
    let stated = ":: tembra :: State < :: tembra :: serde_json :: Value >";
    let root = native(&module, &[]);
    for expected in [
        format!("pub (crate) fn __tembra_service_ledger (orb : & str) -> :: tembra :: Service < (() , {stated}) >"),
        "| declaration | { let state = self :: __tembra_state :: declare :: < Total > (& declaration , 1u32 , \"Total\" , | _ : & Total | :: core :: result :: Result :: Ok (()) ,) ? ; :: core :: result :: Result :: Ok ((() , state)) }".to_owned(),
        format!(". method_with (\"count\" , move | __tembra_object : :: std :: rc :: Rc < (() , {stated}) > , __tembra_context : :: tembra :: Context , () : () | self :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_object . 1 . clone () , async move {{ __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: count ()) }}) , __TEMBRA_CODEC_COUNT ,)"),
        format!(". payload_method (\"issue\" , move | __tembra_object : :: std :: rc :: Rc < (() , {stated}) > , __tembra_context : :: tembra :: Context , __tembra_payload : :: tembra :: portable :: Payload | self :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_object . 1 . clone () , async move {{"),
        format!(", | object : & (() , {stated}) | object . 1 . clone ())"),
        format!("pub (crate) fn __tembra_resource_ticket < E : 'static > (service : :: tembra :: Service < E > , __tembra_state_of : fn (& E) -> {stated}) -> :: tembra :: Service < E >"),
        ". payload_method (\"[constructor]ticket\" , move | __tembra_object : :: std :: rc :: Rc < E > , __tembra_context : :: tembra :: Context , __tembra_payload : :: tembra :: portable :: Payload | self :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_state_of (& __tembra_object) , async move {".to_owned(),
        ". payload_method (\"[method]ticket.sum\" , move | __tembra_object : :: std :: rc :: Rc < E > , __tembra_context : :: tembra :: Context , mut __tembra_payload : :: tembra :: portable :: Payload | self :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_state_of (& __tembra_object) , async move {".to_owned(),
        "pub (crate) fn __tembra_service_shelf (orb : & str) -> :: tembra :: Service < () >".to_owned(),
        ". method_with (\"size\" , | _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Shelf :: size ()) } , __TEMBRA_CODEC_SIZE ,)".to_owned(),
    ] {
        assert!(root.contains(&expected), "{expected}\n{root}");
    }
    let notes = native(&module, &["notes"]);
    for expected in [
        format!("-> :: tembra :: Service < (Notes , {stated}) >"),
        "move | declaration | { let state = self :: super :: __tembra_state :: declare :: < super :: Kept > (& declaration , 2u32 , \"Kept\" , | state : & super :: Kept | state . bounded () ,) ? ; :: core :: result :: Result :: Ok ((Notes :: create (& inputs , & declaration) ? , state)) }".to_owned(),
        format!(". method_with (\"title\" , move | __tembra_object : :: std :: rc :: Rc < (Notes , {stated}) > , __tembra_context : :: tembra :: Context , () : () | self :: super :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_object . 1 . clone () , async move {{ __tembra_context . check () ? ; :: core :: result :: Result :: Ok (__tembra_object . 0 . title ()) }}) , __TEMBRA_CODEC_TITLE ,)"),
        format!(". local_method (\"kept\" , move | __tembra_object : :: std :: rc :: Rc < (Notes , {stated}) > , __tembra_context : :: tembra :: Context , () : () | self :: super :: __tembra_state :: scoped (__tembra_context . clone () , __tembra_object . 1 . clone () , async move {{ __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Notes :: kept ()) }}) ,)"),
    ] {
        assert!(notes.contains(&expected), "{expected}\n{notes}");
    }
    let generated = tokens(&module);
    assert!(
        generated.contains(":: tembra :: __guest ! { pub fn read < T : :: serde :: de :: DeserializeOwned > () -> T { parse (& super :: tembra :: guest :: state :: read ()) }"),
        "{generated}"
    );
    let state = native(&module, &["__tembra_state"]);
    for expected in [
        "pub (crate) fn scoped < F : :: core :: future :: Future > (context : :: tembra :: Context , state : :: tembra :: State < :: tembra :: serde_json :: Value > , future : F ,) -> impl :: core :: future :: Future < Output = F :: Output > {",
        "pub fn read < T : :: serde :: de :: DeserializeOwned > () -> T {",
        "pub fn update < T : :: serde :: Serialize + :: serde :: de :: DeserializeOwned > (value : & T , name : & str ,) -> :: core :: result :: Result < T , String > {",
        "let json = storable (value , name) ? ;",
        "let state = declaration . state (kept (& json) , version , | _ | :: core :: result :: Result :: Ok (())) ? ;",
        "declaration . validate_on_reload (move || {",
    ] {
        assert!(state.contains(expected), "{expected}\n{state}");
    }
}

/// The resources of the Orb's root natively: `orb_named` builds a table of them for the Orb, which
/// the Orb's create fills, declaring each with its version, `#[open]` and `#[close]` before the
/// `#[root]` runs, and which every service takes, and hands the functions of its WIT resources;
/// each method's future, local ones too, and each of those functions', runs in the scope of its
/// Orb, where the resources' `with` reaches the table, in a sub-module through `super::`. The
/// component's `with`, over its slot and the bindings' import, is a component build's only, and a
/// native host gets its own in the component's `__tembra_resource`.
#[test]
fn the_orbs_create_declares_its_root_resources_natively() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[inputs]
            pub struct Inputs { pub title: String }
            #[root]
            fn shelves(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<u32> { Ok(0) }
            #[resource]
            pub struct Counts { pub seen: u32 }
            impl Counts {
                #[open]
                fn open() -> Self { Counts { seen: 0 } }
                #[close]
                fn close(self) {}
            }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn count() -> u32 { Counts::with(|counts| counts.seen) }
                #[local]
                pub fn rewind() {}
            }
            #[wit_resource(service = Ledger)]
            pub struct Ticket;
            impl Ticket {
                #[constructor]
                pub fn new() -> Self { Ticket }
                pub fn seen(&self) -> u32 { Counts::with(|counts| counts.seen) }
            }
            pub mod banner {
                #[resource(version = 2)]
                pub struct Banner { pub text: String }
                impl Banner {
                    #[open]
                    pub fn open() -> Self { Banner { text: String::new() } }
                }
                #[service]
                pub struct Shelf;
                impl Shelf { pub fn size() -> u32 { 0 } }
            }
        }",
    );
    let root = native(&module, &[]);
    for expected in [
        "let resources = :: std :: rc :: Rc :: new (< self :: __tembra_resource :: Resources as :: core :: default :: Default > :: default () ,) ;",
        ". create ({ let inputs = :: std :: rc :: Rc :: clone (& inputs) ; let resources = :: std :: rc :: Rc :: clone (& resources) ; move | declaration | { resources . declare (& declaration) ? ; self :: shelves (& inputs , & declaration) } })",
        ". service (self :: __tembra_service_ledger (name , & resources)) . service (self :: banner :: __tembra_service_shelf (name , & resources))",
        "pub (crate) fn __tembra_service_ledger (orb : & str , resources : & :: std :: rc :: Rc < self :: __tembra_resource :: Resources >) -> :: tembra :: Service < () >",
        ". method_with (\"count\" , { let __tembra_resources = :: std :: rc :: Rc :: clone (resources) ; move | _ , __tembra_context : :: tembra :: Context , () : () | self :: __tembra_resource :: scoped (:: std :: rc :: Rc :: clone (& __tembra_resources) , async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: count ()) }) } , __TEMBRA_CODEC_COUNT ,)",
        ". local_method (\"rewind\" , { let __tembra_resources = :: std :: rc :: Rc :: clone (resources) ; move | _ , __tembra_context : :: tembra :: Context , () : () | self :: __tembra_resource :: scoped (:: std :: rc :: Rc :: clone (& __tembra_resources) , async move { __tembra_context . check () ? ; Ledger :: rewind () ; :: core :: result :: Result :: Ok (()) }) } ,)",
        ", resources)",
        "pub (crate) fn __tembra_resource_ticket < E : 'static > (service : :: tembra :: Service < E > , resources : & :: std :: rc :: Rc < self :: __tembra_resource :: Resources >) -> :: tembra :: Service < E >",
        ". payload_method (\"[method]ticket.seen\" , { let __tembra_resources = :: std :: rc :: Rc :: clone (resources) ; move | _ , __tembra_context : :: tembra :: Context , mut __tembra_payload : :: tembra :: portable :: Payload | self :: __tembra_resource :: scoped (:: std :: rc :: Rc :: clone (& __tembra_resources) , async move {",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let banner = native(&module, &["banner"]);
    for expected in [
        "resources : & :: std :: rc :: Rc < self :: super :: __tembra_resource :: Resources >",
        "| self :: super :: __tembra_resource :: scoped (:: std :: rc :: Rc :: clone (& __tembra_resources) ,",
    ] {
        assert!(banner.contains(expected), "{expected}\n{banner}");
    }
    let generated = tokens(&module);
    assert!(
        generated.contains(":: tembra :: __guest ! { impl Counts { # [doc = \" Use the resource `counts` of this Orb's root, opened by the kernel at activation; the kernel confirms it is open, the value lives in this component.\"] pub fn with < R > (use_ : impl :: core :: ops :: FnOnce (& mut Self) -> R) -> R { let _handle = self :: tembra :: guest :: resources :: read (\"counts\") ;"),
        "{generated}"
    );
    let resources = native(&module, &["__tembra_resource"]);
    for expected in [
        "pub struct Resources { counts : :: core :: cell :: OnceCell < :: tembra :: Resource < Slot < super :: Counts >> > , banner : :: core :: cell :: OnceCell < :: tembra :: Resource < Slot < super :: banner :: Banner >> > , }",
        "let resource = declaration . resource (\"counts\" , 1u32 , || { let slot = Slot :: new () ; slot . open (super :: Counts :: open ()) ; :: core :: result :: Result :: Ok (slot) } , | slot , _ | { if let :: core :: option :: Option :: Some (value) = slot . close () { value . close () ; } :: std :: boxed :: Box :: pin (async { :: core :: result :: Result :: Ok (()) }) } ,) ? ; let _ = self . counts . set (resource) ;",
        "let resource = declaration . resource (\"banner\" , 2u32 , || { let slot = Slot :: new () ; slot . open (super :: banner :: Banner :: open ()) ; :: core :: result :: Result :: Ok (slot) } , | slot , _ | { slot . close () ; :: std :: boxed :: Box :: pin (async { :: core :: result :: Result :: Ok (()) }) } ,) ? ; let _ = self . banner . set (resource) ;",
        "pub (crate) fn scoped < F : :: core :: future :: Future > (resources : :: std :: rc :: Rc < Resources > , future : F ,) -> impl :: core :: future :: Future < Output = F :: Output > {",
        "impl super :: Counts {",
        "pub fn with < R > (use_ : impl :: core :: ops :: FnOnce (& mut Self) -> R) -> R { let slot = current () . counts . get () . and_then (| resource | resource . value () . ok ()) . expect (\"the resource `counts` of the Orb's root is open\") ; slot . with (use_) }",
    ] {
        assert!(resources.contains(expected), "{expected}\n{resources}");
    }
}

/// What the native side does not cover yet is a compile error inside the facade's wrapper, so a
/// component build of the module is unaffected: a WASI import, an import, plain or by key, of an
/// interface with a synchronous function, and a hook declaration, whose `run` is synchronous, both
/// of which work in a component build only, each named.
#[test]
fn the_native_side_refuses_what_it_does_not_cover_yet() {
    let service = "#[service] pub struct Ledger; impl Ledger { pub fn count() -> u32 { 0 } }";
    let pace = consumed("native-pace", PACE_WIT);
    let synchronous = "works in a component build only: its function `now` is synchronous, and natively a method has no fiber to suspend while the kernel completes the call, so a native Orb imports only interfaces whose functions are `async func`";
    for (options, module, message) in [
        (
            Options {
                imports: vec![literal("wasi:random/random@0.3.0")],
                ..options(PACKAGE, None)
            },
            ledger("#[service]"),
            "the import of wasi:random/random@0.3.0 is not supported natively yet".to_owned(),
        ),
        (
            Options {
                imports: vec![literal("example:pace/beat@1.0.0")],
                ..options(PACKAGE, Some(&pace))
            },
            ledger("#[service]"),
            format!("the import of example:pace/beat@1.0.0 {synchronous}"),
        ),
        (
            Options {
                keyed: vec![literal("example:pace/beat@1.0.0")],
                ..options(PACKAGE, Some(&pace))
            },
            ledger("#[service]"),
            format!("the import of example:pace/beat@1.0.0 by key {synchronous}"),
        ),
        (
            options(PACKAGE, None),
            format!(
                "mod book {{ #[hook(composition = series, input = Admission)] pub struct Guard; pub struct Admission {{ pub value: i32 }} {service} }}"
            ),
            "the declaration of the hook example:book/guard@1.0.0 works in a component build only: its `run` is synchronous, and natively a method has no fiber to suspend while the kernel runs the hook's contributions, so a native Orb declares no hook".to_owned(),
        ),
    ] {
        let module =
            orb_with(options, &module).unwrap_or_else(|error| panic!("{message}: {error}"));
        assert_eq!(
            native(&module, &[]),
            format!(":: core :: compile_error ! (\"{message}\") ;"),
            "{message}"
        );
        assert!(wit(&module).starts_with("package "), "{message}");
    }
}

/// A contribution to a hook is added to the native Orb: `Providers` names the Orb that declares
/// the hook, of which the Orb requires the hook with the signature of its callback, and a function
/// in the contribution's module adds it with `contribute_portable`, after the services. Its
/// callback reads the hook's input with the codec, runs the author's function in the Orb's root,
/// with the Orb's resources and the call's context, and writes its answer: the value of a
/// waterfall's result, and the value of a bail's option, or none.
#[test]
fn a_contribution_joins_the_hook_natively() {
    let dir = consumed("native-contribution", ADMISSION_WIT);
    let module = orb_with(
        Options {
            context: true,
            ..options(PACKAGE, Some(&dir))
        },
        "mod book {
            #[resource]
            pub struct Counts(u32);
            impl Counts {
                #[open]
                fn open() -> Self { Counts(0) }
            }
            pub mod filters {
                use super::guard;
                /// Block negative values.
                #[hook(\"example:admission/guard@1.0.0\", priority = -7)]
                pub fn admit(input: guard::Admission) -> guard::Admission { input }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let root = native(&module, &[]);
    for expected in [
        "pub fn orb (providers : Providers) -> :: tembra :: Result < :: tembra :: Orb > { orb_named (\"book\" , 1u32 , providers) }",
        "pub struct Providers { # [doc = \" The Orb that declares the hook `example:admission/guard@1.0.0`, which this Orb contributes to.\"] pub guard : :: tembra :: Provider , }",
        "let requirement = entry (& mut requirements , & providers . guard) ? ; requirement . hooks . insert (:: std :: string :: String :: from (\"example:admission/guard@1.0.0\")) ;",
        "let orb = self :: filters :: __tembra_contribution_admit (orb , & providers . guard . name , & resources) ; :: core :: result :: Result :: Ok (orb)",
        "impl __TembraBoundary for exports :: example :: admission :: guard :: Admission {",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    assert!(!root.contains("__tembra_import"), "{root}");
    let filters = native(&module, &["filters"]);
    for expected in [
        "pub (crate) fn __tembra_contribution_admit (orb : :: tembra :: Orb , target : & str , resources : & :: std :: rc :: Rc < self :: super :: __tembra_resource :: Resources >) -> :: tembra :: Orb { orb . contribute_portable (target , \"example:admission/guard@1.0.0\" ,",
        ", - 7i32 , { let __tembra_resources = :: std :: rc :: Rc :: clone (resources) ; move | __tembra_context : :: tembra :: Context , __tembra_input : :: tembra :: serde_json :: Value | self :: super :: context :: __scoped (__tembra_context . clone () , self :: super :: __tembra_resource :: scoped (:: std :: rc :: Rc :: clone (& __tembra_resources) , async move { __tembra_context . check () ? ;",
        "let input = read (__tembra_input) . map_err (| message | { :: tembra :: Error :: invalid (:: tembra :: core :: Reason :: Arguments , message) }) ? ; let value = admit (input) ;",
        "write (value) . map_err (| message | { :: tembra :: Error :: invalid (:: tembra :: core :: Reason :: Result , message) })",
    ] {
        assert!(filters.contains(expected), "{expected}\n{filters}");
    }
}

/// A package with an interface whose functions are all `async func`, and one with a synchronous
/// function.
const PACE_WIT: &str = "package example:pace@1.0.0;
interface clock {
  record tick { at: u64, label: string }
  /// Wait until `value` and return it.
  wait: async func(value: u64) -> u64;
  last: async func(labels: list<string>) -> option<tick>;
}
interface beat {
  now: func() -> u64;
  later: async func() -> u64;
}
";

/// An import of an interface whose functions are all `async func` is served natively: `orb(..)`
/// takes `Providers`, one `tembra::Provider` per import, from which `orb_named` builds the Orb's
/// requirements and the table of its imports, whose reference to the provider's `main` object the
/// Orb's create declares, and which each service takes. The Orb module names, in place of the
/// bindings' import module, a module of the same name, which re-exports its types and whose
/// `async` functions take their parameters owned and call the provider; a component build alone
/// re-exports the bindings' module. A type of the interface reads and writes its JSON as a type
/// of an implemented one does.
#[test]
fn an_import_of_async_functions_is_served_natively() {
    let dir = consumed("native-import", PACE_WIT);
    let module = orb_with(
        Options {
            imports: vec![literal("example:pace/clock@1.0.0")],
            ..options(PACKAGE, Some(&dir))
        },
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub async fn count() -> u64 { clock::wait(1).await }
                pub fn latest(tick: clock::Tick) -> clock::Tick { tick }
            }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let root = native(&module, &[]);
    for expected in [
        "pub fn orb (providers : Providers) -> :: tembra :: Result < :: tembra :: Orb > { orb_named (\"book\" , 1u32 , providers) }",
        "pub fn orb_named (name : & str , api_version : u32 , providers : Providers) -> :: tembra :: Result < :: tembra :: Orb >",
        "let imports = :: std :: rc :: Rc :: new (self :: __tembra_import :: Imports :: new (& providers)) ;",
        "for requirement in self :: __tembra_providers :: requirements (& providers) ? { orb = orb . requires (requirement) ; }",
        "move | declaration | { imports . declare (& declaration) ? ; :: core :: result :: Result :: Ok (()) }",
        ". service (self :: __tembra_service_ledger (name , & imports))",
        "pub (crate) fn __tembra_service_ledger (orb : & str , imports : & :: std :: rc :: Rc < self :: __tembra_import :: Imports >) -> :: tembra :: Service < () >",
        "self :: __tembra_import :: scoped (__tembra_context . clone () , :: std :: rc :: Rc :: clone (& __tembra_imports) , async move {",
        "pub struct Providers { # [doc = \" The Orb answering the import of `example:pace/clock@1.0.0`, at its `main` object.\"] pub clock : :: tembra :: Provider , }",
        "let requirement = entry (& mut requirements , & providers . clock) ? ; let service = :: std :: format ! (\"{}::{}\" , providers . clock . name , \"example:pace/clock@1.0.0\") ;",
        "pub mod clock { # [allow (unused_imports)] pub use super :: example :: pace :: clock :: * ;",
        "# [doc = \" Wait until `value` and return it.\"] pub async fn wait (value : u64) -> u64 { let __tembra_call = super :: __tembra_import :: current () ;",
        "pub async fn last (labels : :: std :: vec :: Vec < :: std :: string :: String >) -> :: core :: option :: Option < super :: example :: pace :: clock :: Tick > {",
        ". call (& __tembra_call . imports . clock . reference () , \"last\" , __tembra_arguments , | value |",
        "impl __TembraBoundary for self :: example :: pace :: clock :: Tick {",
        ". method_with (\"latest\" ,",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    assert!(
        tokens(&module)
            .contains(":: tembra :: __guest ! { pub use self :: example :: pace :: clock ; }"),
        "{}",
        tokens(&module)
    );
}

/// A method returning `tembra::Result<T>`, its error channel, has `T` as its WIT result, nothing
/// for `()`. Its native handle answers with the author's result, whose failure is the call's
/// error, and its codec writes `T`; the component's bridge, which a host build type-checks, takes
/// the value out, in a derived and in an implemented interface. Any other `Result` is a WIT
/// `result`, and a `Result` of one type argument, the runtime's imported by `use`, stands for no
/// `tembra::Result`: the refusal says to spell it by its path.
#[test]
fn a_tembra_result_is_the_error_channel_of_a_native_method() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn divide(a: u32, b: u32) -> tembra::Result<u32> { todo!() }
                pub async fn settle() -> ::tembra::Result<()> { todo!() }
                pub fn quotient(a: u32, b: u32) -> Result<u32, String> { todo!() }
            }
        }",
    );
    let wit = wit(&module);
    for expected in [
        "divide: func(a: u32, b: u32) -> u32;",
        "settle: async func();",
        "quotient: func(a: u32, b: u32) -> result<u32, string>;",
    ] {
        assert!(wit.contains(expected), "{expected}\n{wit}");
    }
    let native = native(&module, &[]);
    for expected in [
        "const __TEMBRA_CODEC_DIVIDE : :: tembra :: Codec < (u32 , u32 ,) , u32 >",
        "const __TEMBRA_CODEC_SETTLE : :: tembra :: Codec < () , () >",
        "| _ , __tembra_context : :: tembra :: Context , (a , b ,) : (u32 , u32 ,) | async move { __tembra_context . check () ? ; Ledger :: divide (a , b) } , __TEMBRA_CODEC_DIVIDE ,",
        "| _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; Ledger :: settle () . await } , __TEMBRA_CODEC_SETTLE ,",
        "async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: quotient (a , b)) }",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    let generated = tokens(&module);
    for expected in [
        "fn divide (a : u32 , b : u32) -> u32 { match Ledger :: divide (a , b) { :: core :: result :: Result :: Ok (value) => value , :: core :: result :: Result :: Err (error) => :: core :: panic ! (\"{}\" , error) , } }",
        "async fn settle () { match Ledger :: settle () . await { :: core :: result :: Result :: Ok (value) => value ,",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let dir = consumed("fallible-implemented", COUNTER_WIT);
    let module = orb_with(
        options("example:tally@1.0.0", Some(&dir)),
        "mod tally {
            #[service(\"example:counters/counter@1.0.0\")]
            pub struct Counter;
            impl Counter { pub fn add(amount: u32) -> tembra::Result<u32> { todo!() } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let implemented = self::native(&module, &[]);
    assert!(
        implemented.contains(
            "async move { __tembra_context . check () ? ; Counter :: add (amount) } , __TEMBRA_CODEC_ADD ,"
        ),
        "{implemented}"
    );
    let generated = tokens(&module);
    assert!(
        generated.contains(
            "fn add (amount : u32) -> u32 { match Counter :: add (amount) { :: core :: result :: Result :: Ok (value) => value ,"
        ),
        "{generated}"
    );
    let refused = refused(
        PACKAGE,
        "mod book {
            use tembra::Result;
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn divide(a: u32) -> Result<u32> { todo!() } }
        }",
    );
    assert!(
        refused.contains(
            "the macro reads `Result` by its name as `Result<T, E>`, so another `Result`, such as a module's own alias, cannot stand for it here; for a service method's error channel, spell the runtime's by its path, `tembra::Result<T>`"
        ),
        "{refused}"
    );
}

/// A type of an interface the module implements is the bindings' type, which reads and writes its
/// JSON through the boundary trait, implemented in the Orb module from the interface's WIT: a
/// record by the bindings' fields, keyed by the lowerCamelCase form of their WIT names, an enum by
/// the WIT names of its cases, a variant as `{tag, val}`, a flags, which the bindings generate with
/// `bitflags`, as an object of the flags set. Each is named from the module of the interface that
/// defines it, under `exports` for the implemented one and at the root for one it `use`s, which the
/// world imports; an alias of a list reads as the list. A method of a sub-module and a record of
/// the module name them as the author does, through the implemented interface's module, and a type
/// holding a resource is refused, naming what it holds.
#[test]
fn a_type_of_an_implemented_interface_gets_its_codec_from_the_wit() {
    let dir = consumed(
        "native-implemented-types",
        "package example:ledger@1.0.0;
        interface money {
          enum currency { eur, usd }
        }
        interface book {
          use money.{currency};
          flags marks { audited, disputed-once }
          record entry { label: string, amount: s64, currency: currency, marks: marks }
          variant outcome { recorded(u32), refused(string), empty }
          type entries = list<entry>;
          add: func(entry: entry) -> outcome;
          all: func() -> entries;
        }
        world keeper { export book; }",
    );
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            pub mod books {
                #[service(\"example:ledger/book@1.0.0\")]
                pub struct Keeper;
                impl Keeper {
                    pub fn add(entry: super::book::Entry) -> super::book::Outcome { todo!() }
                    pub fn all() -> super::book::Entries { todo!() }
                }
            }
            pub struct Order { pub entry: book::Entry, pub count: u32 }
            #[service]
            pub struct Orders;
            impl Orders { pub fn place(order: Order) -> u32 { 0 } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let root = native(&module, &[]);
    for expected in [
        "impl __TembraBoundary for exports :: example :: ledger :: book :: Entry {",
        "label : match object . remove (\"label\") { :: core :: option :: Option :: Some (item) => :: tembra :: serde_json :: from_value :: < :: std :: string :: String > (item)",
        "currency : match object . remove (\"currency\") { :: core :: option :: Option :: Some (item) => < self :: example :: ledger :: money :: Currency as __TembraBoundary > :: decode (item) ?",
        "let Self { label : __tembra_0 , amount : __tembra_1 , currency : __tembra_2 , marks : __tembra_3 } = self ;",
        "impl __TembraBoundary for self :: example :: ledger :: money :: Currency {",
        "\"usd\" => :: core :: result :: Result :: Ok (Self :: Usd) ,",
        "impl __TembraBoundary for exports :: example :: ledger :: book :: Marks {",
        "let mut flags = Self :: empty () ;",
        "\"disputedOnce\" => Self :: DISPUTED_ONCE ,",
        ":: tembra :: serde_json :: Value :: Bool (set) => flags . set (flag , set) ,",
        "if self . contains (Self :: DISPUTED_ONCE) { flags . insert (:: std :: string :: String :: from (\"disputedOnce\") , :: tembra :: serde_json :: Value :: Bool (true) ,) ; }",
        "impl __TembraBoundary for exports :: example :: ledger :: book :: Outcome {",
        "(\"recorded\" , :: core :: option :: Option :: Some (item)) => :: tembra :: serde_json :: from_value :: < u32 > (item) . map_err (| error | error . to_string ()) . map (Self :: Recorded) ,",
        "(\"empty\" , :: core :: option :: Option :: None) => :: core :: result :: Result :: Ok (Self :: Empty) ,",
        // A record of the module holding one names it as the author does.
        "impl __TembraBoundary for Order {",
        "entry : match object . remove (\"entry\") { :: core :: option :: Option :: Some (item) => < exports :: example :: ledger :: book :: Entry as __TembraBoundary > :: decode (item) ?",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let books = native(&module, &["books"]);
    for expected in [
        "const __TEMBRA_CODEC_ADD : :: tembra :: Codec < (super :: book :: Entry ,) , super :: book :: Outcome >",
        "< super :: exports :: example :: ledger :: book :: Entry as super :: __TembraBoundary > :: decode (__tembra_0) ?",
        "< super :: exports :: example :: ledger :: book :: Outcome as super :: __TembraBoundary > :: encode (value)",
        "value . into_iter () . map (| item | < super :: exports :: example :: ledger :: book :: Entry as super :: __TembraBoundary > :: encode (item))",
    ] {
        assert!(books.contains(expected), "{expected}\n{books}");
    }
    let dir = consumed(
        "native-implemented-resource",
        "package example:ledger@1.0.0;
        interface texts {
          resource builder;
        }
        interface book {
          use texts.{builder};
          record entry { label: string, builder: builder }
          add: func(entry: entry) -> u32;
        }
        world keeper { export book; }",
    );
    let module = orb_with(
        options("example:keeper@1.0.0", Some(&dir)),
        "mod keeper {
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper { pub fn add(entry: book::Entry) -> u32 { 0 } }
        }",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        native(&module, &[]),
        ":: core :: compile_error ! (\"`Keeper::add` takes `book::Entry`, a type of the implemented interface example:ledger/book@1.0.0 that is or holds a resource, which the native side does not cover yet\") ;"
    );
}

/// A nested option is refused wherever a method reaches it, in its own types too, for both
/// targets: the boundary's JSON writes `None` and `Some(None)` alike, as `null`, which is why the
/// runtime refuses it in a signature, and the macro refuses it where it is written, so the native
/// side never sees one.
#[test]
fn a_nested_option_is_refused_wherever_a_method_reaches_it() {
    for (items, function) in [
        ("", "pub fn add(count: Option<Option<u32>>) -> u32 { 0 }"),
        (
            "",
            "pub fn list() -> Vec<Option<Option<String>>> { Vec::new() }",
        ),
        (
            "pub struct Entry { pub count: Option<Option<u32>> }",
            "pub fn add(entry: Entry) -> u32 { 0 }",
        ),
        (
            "pub enum Shape { Dot, Line(Option<Option<u32>>) }",
            "pub fn draw(shape: Shape) {}",
        ),
        (
            "pub enum Shape { Dot, Line { length: (u32, Option<Option<u32>>) } }",
            "pub fn draw(shape: Shape) {}",
        ),
    ] {
        let error = refused(
            PACKAGE,
            &format!(
                "mod book {{ {items} #[service] pub struct Ledger; impl Ledger {{ {function} }} }}"
            ),
        );
        assert!(
            error.contains("`Option<Option<T>>` is an option of an option, which the runtime refuses in a signature: use an enum of the module with a case for each"),
            "{items} {function}: {error}"
        );
    }
}

/// Free JSON, `serde_json::Value` by its path, is a WIT `string` that holds its JSON text. The
/// component's bridges spell it as the bindings' string, from `alloc`, and read or write its text
/// wherever a method holds it; a record holding it stays the author's, converted to and from the
/// bindings' record of its text. The native codec reads and writes the text. A bare `Value` names
/// a type of the module, which the refusal says to write by its path, and free JSON beside a
/// resource, or in a derived hook's input, is refused.
#[test]
fn free_json_is_a_string_that_holds_its_text() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub struct Note { pub title: String, pub body: serde_json::Value }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn annotated(document: serde_json::Value, notes: Vec<Option<::serde_json::Value>>) -> serde_json::Value { todo!() }
                pub fn noted(note: Note) -> (Note, serde_json::Value) { todo!() }
            }
        }",
    );
    let wit = wit(&module);
    for expected in [
        "record note {\n    title: string,\n    body: string,\n  }",
        "annotated: func(document: string, notes: list<option<string>>) -> string;",
        "noted: func(note: note) -> tuple<note, string>;",
    ] {
        assert!(wit.contains(expected), "{expected}\n{wit}");
    }
    let generated = tokens(&module);
    for expected in [
        "extern crate alloc as __tembra_alloc ;",
        "fn annotated (document : __tembra_alloc :: string :: String , notes : Vec < Option < __tembra_alloc :: string :: String > >) -> __tembra_alloc :: string :: String {",
        "match serde_json :: from_str :: < serde_json :: Value > (& document) { :: core :: result :: Result :: Ok (json) => json , :: core :: result :: Result :: Err (error) => :: core :: panic ! (\"{}\" , error) , }",
        "notes . into_iter () . map (| item | item . map (| item | match :: serde_json :: from_str :: < :: serde_json :: Value > (& item) {",
        "match serde_json :: to_string (& Ledger :: annotated (",
        "{ :: core :: result :: Result :: Ok (text) => text , :: core :: result :: Result :: Err (error) => :: core :: panic ! (\"{}\" , error) , }",
        "fn noted (note : exports :: example :: book :: ledger :: Note) -> (exports :: example :: book :: ledger :: Note , __tembra_alloc :: string :: String ,) {",
        "body : match serde_json :: to_string (& value . body) {",
        "body : match serde_json :: from_str :: < serde_json :: Value > (& value . body) {",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let native = native(&module, &[]);
    for expected in [
        ":: tembra :: serde_json :: Value :: String (text) => { :: tembra :: serde_json :: from_str :: < serde_json :: Value > (& text) . map_err (| error | :: std :: format ! (\"expected JSON text: {}\" , error)) }",
        "\"expected JSON text as a string\"",
        ":: tembra :: serde_json :: to_string (& value) . map (:: tembra :: serde_json :: Value :: String) . map_err (| error | error . to_string ())",
        "impl __TembraBoundary for Note {",
        "body : match object . remove (\"body\") {",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    for (module, message) in [
        (
            "mod book {
                use serde_json::Value;
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn read(document: Value) {} }
            }",
            "`Value` is used by a service method or a hook but is not a struct or enum of this module; for free JSON, write it by its path, `serde_json::Value`",
        ),
        (
            "mod book {
                #[wit_resource]
                pub struct Ticket;
                impl Ticket { #[constructor] pub fn new() -> Self { Ticket } }
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn issue() -> (Ticket, serde_json::Value) { todo!() } }
            }",
            "which holds the resource `Ticket` and free JSON, `serde_json::Value`",
        ),
        (
            "mod book {
                #[hook(composition = series, input = Event)]
                pub struct Audit;
                pub struct Event { pub data: serde_json::Value }
                #[service]
                pub struct Ledger;
                impl Ledger { pub fn count() -> u32 { 0 } }
            }",
            "a derived hook's input holds free JSON",
        ),
    ] {
        let error = refused(PACKAGE, module);
        assert!(error.contains(message), "{message}\n{error}");
    }
}

/// A newtype, a tuple struct of one field, is spelled in the WIT as its field's type wherever a
/// type that enters the WIT names it, and stays the author's struct. In a component, each newtype
/// implements a hidden trait next to it, whose type is the bindings' type of its field: a bridge
/// spells the newtype as that type, reads it into the newtype through `From` of the field's type,
/// by a second hidden trait whose diagnostic names a newtype only `TryFrom` makes, and writes the
/// field back; a host build, whose bridges never run, reads it through `TryFrom`. A record holding
/// a newtype stays the author's, converted field by field. The native codec reads the field's JSON
/// into the newtype through `TryFrom`, whose refusal fails the call, and writes the field.
#[test]
fn a_newtype_is_spelled_as_its_field() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub mod ids {
                pub struct Code(String);
                impl TryFrom<String> for Code {
                    type Error = String;
                    fn try_from(code: String) -> Result<Self, String> { Ok(Self(code)) }
                }
            }
            pub struct Entry { pub code: ids::Code, pub amount: Amount }
            pub struct Amount(u64);
            impl From<u64> for Amount { fn from(amount: u64) -> Self { Self(amount) } }
            pub struct Tagged(Entry);
            impl From<Entry> for Tagged { fn from(entry: Entry) -> Self { Self(entry) } }
            pub struct Total(Amount);
            impl From<Amount> for Total { fn from(amount: Amount) -> Self { Self(amount) } }
            pub mod documents {
                pub struct Document(serde_json::Value);
                impl From<serde_json::Value> for Document {
                    fn from(value: serde_json::Value) -> Self { Self(value) }
                }
            }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn add(entry: Entry, codes: Vec<ids::Code>) -> Option<Amount> { todo!() }
                pub fn tagged(tag: Tagged) -> (ids::Code, u32) { todo!() }
                pub fn total(document: documents::Document) -> Total { todo!() }
            }
        }",
    );
    let wit = wit(&module);
    for expected in [
        "record entry {\n    code: string,\n    amount: u64,\n  }",
        "add: func(entry: entry, codes: list<string>) -> option<u64>;",
        "tagged: func(tag: entry) -> tuple<string, u32>;",
        "total: func(document: string) -> u64;",
    ] {
        assert!(wit.contains(expected), "{expected}\n{wit}");
    }
    let generated = tokens(&module);
    for expected in [
        "# [allow (dead_code)] trait __TembraNewtype : :: core :: marker :: Sized {",
        // A component reads a newtype through `From`, by a trait whose diagnostic names one that
        // implements no `From`.
        "# [diagnostic :: on_unimplemented (message = \"`{Self}` is native only: a component reads it from its field through `From<{Field}>`, which `{Self}` does not implement\" , label = \"a newtype a component takes, which only `TryFrom` makes\" , note = \"a component has no error channel for an argument refused before its method runs, only a trap, which would retire its revision for every caller until a reload: implement `From<{Field}>` for `{Self}`, or take it natively only, where a value its `TryFrom` refuses fails that call alone\")] # [allow (dead_code)] trait __TembraFromField < Field > : :: core :: marker :: Sized {",
        "impl < Field , Newtype : :: core :: convert :: From < Field > > __TembraFromField < Field > for Newtype { fn __tembra_from_field (field : Field) -> Self { :: core :: convert :: From :: from (field) } }",
        // Each newtype in its own module, where its field and the field's type are the author's:
        // through `From` in a component, through `TryFrom` in a host build.
        "impl super :: __TembraNewtype for Code { type Bindings = String ; fn __tembra_from_bindings (value : Self :: Bindings) -> Self { # [cfg (target_arch = \"wasm32\")] { < Self as super :: __TembraFromField < String > > :: __tembra_from_field (:: core :: convert :: Into :: into (value)) } # [cfg (not (target_arch = \"wasm32\"))] { match < Self as :: core :: convert :: TryFrom < String > > :: try_from (:: core :: convert :: Into :: into (value)) { :: core :: result :: Result :: Ok (value) => value , :: core :: result :: Result :: Err (error) => :: core :: panic ! (\"{}\" , error) , } } } fn __tembra_into_bindings (self) -> Self :: Bindings { :: core :: convert :: Into :: into (self . 0) } }",
        "impl __TembraNewtype for Amount { type Bindings = u64 ;",
        // A newtype of a type the macro converts holds the bindings' type, from an interface
        // that names it, a newtype of a newtype that one's, and a newtype of free JSON a string,
        // from the `alloc` of its module.
        "impl __TembraNewtype for Tagged { type Bindings = exports :: example :: book :: ledger :: Entry ;",
        "impl __TembraNewtype for Total { type Bindings = < Amount as __TembraNewtype > :: Bindings ; fn __tembra_from_bindings (value : Self :: Bindings) -> Self { # [cfg (target_arch = \"wasm32\")] { < Self as __TembraFromField < Amount > > :: __tembra_from_field (< Amount as __TembraNewtype > :: __tembra_from_bindings (value)) }",
        "impl super :: __TembraNewtype for Document { type Bindings = __tembra_alloc :: string :: String ;",
        // The bridges spell a newtype as the bindings' type of its field and convert it.
        "fn add (entry : exports :: example :: book :: ledger :: Entry , codes : Vec < < ids :: Code as __TembraNewtype > :: Bindings >) -> Option < < Amount as __TembraNewtype > :: Bindings > {",
        "codes . into_iter () . map (| item | < ids :: Code as __TembraNewtype > :: __tembra_from_bindings (item)) . collect ()",
        "fn tagged (tag : < Tagged as __TembraNewtype > :: Bindings) -> (< ids :: Code as __TembraNewtype > :: Bindings , u32 ,) {",
        // A record holding a newtype stays the author's, converted field by field.
        "code : < ids :: Code as __TembraNewtype > :: __tembra_into_bindings (value . code)",
        "code : < ids :: Code as __TembraNewtype > :: __tembra_from_bindings (value . code)",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let root = native(&module, &[]);
    for expected in [
        "impl __TembraBoundary for Amount { fn decode (value : :: tembra :: serde_json :: Value ,) -> :: core :: result :: Result < Self , :: std :: string :: String > { < Self as :: core :: convert :: TryFrom < u64 >> :: try_from (:: tembra :: serde_json :: from_value :: < u64 > (value) . map_err (| error | error . to_string ()) ?) . map_err (| error | :: std :: string :: ToString :: to_string (& error)) }",
        "impl __TembraBoundary for Tagged {",
        "< Self as :: core :: convert :: TryFrom < Entry >> :: try_from (< Entry as __TembraBoundary > :: decode (value) ?)",
        "< Entry as __TembraBoundary > :: encode (self . 0)",
        "< ids :: Code as __TembraBoundary > :: decode (item)",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let ids = native(&module, &["ids"]);
    assert!(
        ids.contains("impl super :: __TembraBoundary for Code {"),
        "{ids}"
    );
    let documents = items(&module)
        .iter()
        .find_map(|item| match item {
            Item::Mod(inner) if inner.ident == "documents" => {
                Some(quote::quote!(#inner).to_string())
            }
            _ => None,
        })
        .expect("the module `documents`");
    assert!(
        documents.contains("extern crate alloc as __tembra_alloc ;"),
        "{documents}"
    );
    // A tuple struct no type that enters the WIT names, or of a name another type takes, stays
    // ordinary Rust, with no conversion of its own.
    let module = expanded(
        PACKAGE,
        "mod book {
            struct Dropped(core::cell::Cell<u32>);
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }
        }",
    );
    assert!(!tokens(&module).contains("__TembraNewtype"));
    let module = expanded(
        PACKAGE,
        "mod book {
            pub mod a { pub struct Code(pub String); }
            pub mod b { pub struct Code { pub text: String } }
            pub use b::Code;
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn add(code: Code) {} }
        }",
    );
    let wit = self::wit(&module);
    assert!(
        wit.contains("record code {\n    text: string,\n  }\n  add: func(code: code);"),
        "{wit}"
    );
    assert!(!tokens(&module).contains("__TembraNewtype"));
}

/// What a newtype cannot be is refused: a newtype holding a resource, which the conversions do
/// not carry, a newtype beside a resource in one parameter or result, a newtype in a derived
/// hook's input, which has two types in the bindings, and a newtype holding itself, which WIT
/// cannot spell.
#[test]
fn what_a_newtype_cannot_be_is_refused() {
    let resource = "#[wit_resource]
        pub struct Ticket;
        impl Ticket { #[constructor] pub fn new() -> Self { Ticket } }";
    for (items, function, message) in [
        (
            "pub struct Held(Ticket);",
            "pub fn issue() -> Held { todo!() }",
            "the newtype `Held` holds the resource `Ticket`",
        ),
        (
            "pub struct Code(String);",
            "pub fn issue() -> (Ticket, Code) { todo!() }",
            "`Ledger::issue` returns `(Ticket , Code)`, which holds the resource `Ticket` and the newtype `Code`",
        ),
        (
            "pub struct Chain(Vec<Link>); pub struct Link(Chain);",
            "pub fn chain() -> Chain { todo!() }",
            "`Chain` holds itself through its field, which WIT cannot spell",
        ),
    ] {
        let error = refused(
            PACKAGE,
            &format!(
                "mod book {{ {resource} {items} #[service] pub struct Ledger; impl Ledger {{ {function} }} }}"
            ),
        );
        assert!(error.contains(message), "{message}\n{error}");
    }
    for module in [
        "mod book {
            pub struct Code(String);
            #[hook(composition = series, input = Code)]
            pub struct Audit;
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }
        }",
        "mod book {
            pub struct Code(String);
            pub struct Event { pub code: Code }
            #[hook(composition = series, input = Event)]
            pub struct Audit;
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count() -> u32 { 0 } }
        }",
    ] {
        let error = refused(PACKAGE, module);
        assert!(
            error.contains("a derived hook's input holds the newtype `Code`"),
            "{error}"
        );
    }
}

/// Each codec is declared inside the function registering its service, so two services whose
/// names and methods meet in one name, `Ledger::count_words` and `LedgerCount::words`, each keep
/// their own: the module's functions and constants are only `orb()`, `orb_named`, the manifest's
/// constant and the registrations.
#[test]
fn a_codec_is_declared_inside_the_registration_of_its_service() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn count_words(text: String) -> u32 { 0 } }
            #[service]
            pub struct LedgerCount;
            impl LedgerCount { pub fn words(text: String) -> u64 { 0 } }
        }",
    );
    let native = native(&module, &[]);
    let items = syn::parse_str::<syn::File>(&native).unwrap().items;
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) => Some(function.sig.ident.to_string()),
            Item::Const(constant) => Some(constant.ident.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        [
            "_",
            "orb",
            "orb_named",
            "__tembra_service_ledger",
            "__tembra_service_ledger_count"
        ]
    );
    for expected in [
        "fn __tembra_service_ledger (orb : & str) -> :: tembra :: Service < () > { const __TEMBRA_CODEC_COUNT_WORDS : :: tembra :: Codec < (String ,) , u32 >",
        "fn __tembra_service_ledger_count (orb : & str) -> :: tembra :: Service < () > { const __TEMBRA_CODEC_WORDS : :: tembra :: Codec < (String ,) , u64 >",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
}

/// Each type of the module that enters the WIT reads and writes its JSON through a hidden trait of
/// the Orb module, implemented in the type's own module: a record by the bindings' fields, keyed
/// by the lowerCamelCase form of their WIT names; an enum by the WIT names of its cases; a variant
/// as `{tag, val}`; the author's enum with a case with named fields by its own cases and fields;
/// the author's flags by its fields, writing only the flags set. A method's codec reads and writes
/// them through the trait, as the author names them, and a `Result` as `{tag, val}`.
#[test]
fn each_type_of_the_module_gets_its_json() {
    let module = expanded(
        PACKAGE,
        "mod book {
            pub mod shapes {
                pub struct Point { pub x: i64, pub line_width: f32 }
                pub enum Heading { North, SouthWest }
            }
            pub enum Mark { Dot, Cross(shapes::Point), Label { text: String, at: shapes::Point } }
            #[flags]
            pub struct Pen { pub filled: bool, pub dashed_line: bool }
            pub enum Glyph { Blank, Stroke(Pen) }
            #[service]
            pub struct Ledger;
            impl Ledger {
                pub fn place(mark: Mark, heading: shapes::Heading) -> Result<shapes::Point, String> { todo!() }
                pub fn draw(glyph: Glyph) -> Result<(), ()> { todo!() }
            }
        }",
    );
    let root = native(&module, &[]);
    let shapes = native(&module, &["shapes"]);
    for expected in [
        "# [allow (dead_code)] trait __TembraBoundary : :: core :: marker :: Sized { # [doc = r\" Read a value from its JSON, or refuse it with a message.\"] fn decode (value : :: tembra :: serde_json :: Value ,) -> :: core :: result :: Result < Self , :: std :: string :: String > ;",
        // The author's enum, the macro converts, by its own cases and fields.
        "impl __TembraBoundary for Mark {",
        "(\"cross\" , :: core :: option :: Option :: Some (item)) => < shapes :: Point as __TembraBoundary > :: decode (item) . map (Self :: Cross) ,",
        "(\"label\" , :: core :: option :: Option :: Some (item)) => match item { :: tembra :: serde_json :: Value :: Object (mut object) => { let record = Self :: Label { text : match object . remove (\"text\") {",
        "(\"dot\" , :: core :: option :: Option :: None) => :: core :: result :: Result :: Ok (Self :: Dot) ,",
        "Self :: Label { text : __tembra_0 , at : __tembra_1 } =>",
        // The author's flags, by its fields.
        "impl __TembraBoundary for Pen {",
        "\"dashedLine\" => & mut flags . dashed_line ,",
        "if self . dashed_line { flags . insert (:: std :: string :: String :: from (\"dashedLine\") , :: tembra :: serde_json :: Value :: Bool (true) ,) ; }",
        // The bindings' variant, by the bindings' cases.
        "impl __TembraBoundary for Glyph {",
        "Self :: Stroke (item) =>",
        "< Pen as __TembraBoundary > :: encode (item) ?",
        // The methods' codecs, through the trait, as the method names the types.
        "< Mark as __TembraBoundary > :: decode (__tembra_0) ?",
        "< shapes :: Heading as __TembraBoundary > :: decode (__tembra_1) ?",
        ":: core :: result :: Result :: Ok (item) => :: core :: result :: Result :: < :: tembra :: serde_json :: Value , :: std :: string :: String > :: Ok (:: tembra :: serde_json :: Value :: Object (:: core :: iter :: FromIterator :: from_iter ([(:: std :: string :: String :: from (\"tag\") , :: tembra :: serde_json :: Value :: String (:: std :: string :: String :: from (\"ok\")) ,) , (:: std :: string :: String :: from (\"val\") , < shapes :: Point as __TembraBoundary > :: encode (item) ?) ,])))",
        ":: core :: result :: Result :: Err (()) => :: core :: result :: Result :: < :: tembra :: serde_json :: Value , :: std :: string :: String > :: Ok (:: tembra :: serde_json :: Value :: Object (:: core :: iter :: FromIterator :: from_iter ([(:: std :: string :: String :: from (\"tag\") , :: tembra :: serde_json :: Value :: String (:: std :: string :: String :: from (\"err\")) ,) ,])))",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    for expected in [
        // The bindings' record, by the bindings' fields and lowerCamelCase keys.
        "impl super :: __TembraBoundary for Point {",
        "line_width : match object . remove (\"lineWidth\") { :: core :: option :: Option :: Some (item) =>",
        "\"expected a finite f32 other than -0, got {}\"",
        "\"expected an integer, got -0\"",
        "let Self { x : __tembra_0 , line_width : __tembra_1 } = self ;",
        // The bindings' enum, by the WIT names of its cases.
        "impl super :: __TembraBoundary for Heading {",
        "\"south-west\" => :: core :: result :: Result :: Ok (Self :: SouthWest) ,",
        "Self :: SouthWest => \"south-west\" ,",
    ] {
        assert!(shapes.contains(expected), "{expected}\n{shapes}");
    }
}

/// Floats cross as the boundary carries them: a finite number other than `-0`, which an `f32`
/// reads only when it narrows to such a number too, and writes as the `f64` it widens to; the
/// codec never lets serde write `null` for a number it cannot write.
#[test]
fn a_float_crosses_only_when_finite_and_no_negative_zero() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[service]
            pub struct Ledger;
            impl Ledger { pub fn ratio(a: f32, b: f64) -> f32 { 0.0 } }
        }",
    );
    let native = native(&module, &[]);
    for expected in [
        "match __tembra_0 { :: tembra :: serde_json :: Value :: Number (number) => match number . as_f64 () { :: core :: option :: Option :: Some (wide) if (wide . is_finite () && ! (wide == 0.0 && wide . is_sign_negative ())) => { let narrow = wide as f32 ; if (narrow . is_finite () && ! (narrow == 0.0 && narrow . is_sign_negative ())) { :: core :: result :: Result :: Ok (narrow) } else { :: core :: result :: Result :: Err (:: std :: format ! (\"expected a finite f32 other than -0, got {}\" , number)) } }",
        "item => :: tembra :: serde_json :: from_value :: < f32 > (item) . map_err (| error | error . to_string ()) ,",
        ":: core :: option :: Option :: Some (wide) if (wide . is_finite () && ! (wide == 0.0 && wide . is_sign_negative ())) => :: core :: result :: Result :: Ok (wide) , _ => :: core :: result :: Result :: Err (:: std :: format ! (\"expected a finite f64 other than -0, got {}\" , number)) ,",
        "encode : | value | { { let wide = :: core :: convert :: Into :: < f64 > :: into (value) ; match :: tembra :: serde_json :: Number :: from_f64 (wide) { :: core :: option :: Option :: Some (number) if (wide . is_finite () && ! (wide == 0.0 && wide . is_sign_negative ())) => { :: core :: result :: Result :: Ok (:: tembra :: serde_json :: Value :: Number (number)) } _ => :: core :: result :: Result :: Err (:: std :: format ! (\"cannot write {}: the boundary carries finite numbers other than -0\" , wide)) , } } }",
    ] {
        assert!(native.contains(expected), "{expected}\n{native}");
    }
    assert!(!native.contains("to_value"), "{native}");
}

/// A struct marked `#[inputs]` is the argument of `orb(..)`, and of `orb_named(..)` after the name
/// and the API version: the author's struct, as written but for the marker, whatever its fields
/// hold, and no type of the WIT. In a sub-module, the Orb module names it by its path.
#[test]
fn the_inputs_are_the_argument_of_orb() {
    let service = "#[service] pub struct Ledger; impl Ledger { pub fn count() -> u32 { 0 } }";
    let module = expanded(
        PACKAGE,
        &format!(
            "mod book {{
                use std::rc::Rc;
                #[inputs]
                pub struct Inputs {{ pub label: String, pub scale: Rc<dyn Fn(u32) -> u32> }}
                {service}
            }}"
        ),
    );
    let root = native(&module, &[]);
    for expected in [
        "pub fn orb (inputs : Inputs) -> :: tembra :: Result < :: tembra :: Orb > { orb_named (\"book\" , 1u32 , inputs) }",
        "pub fn orb_named (name : & str , api_version : u32 , inputs : Inputs) -> :: tembra :: Result < :: tembra :: Orb >",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let tokens = tokens(&module);
    assert!(
        tokens.contains(
            "pub struct Inputs { pub label : String , pub scale : Rc < dyn Fn (u32) -> u32 > }"
        ),
        "{tokens}"
    );
    assert!(!tokens.contains("inputs ]"), "{tokens}");
    assert!(!wit(&module).contains("record"), "{}", wit(&module));
    let module = expanded(
        PACKAGE,
        &format!(
            "mod book {{ pub mod host {{ #[inputs] pub struct Lent {{ pub label: String }} }} {service} }}"
        ),
    );
    let root = native(&module, &[]);
    assert!(
        root.contains("pub fn orb (inputs : host :: Lent)"),
        "{root}"
    );
}

/// An Orb has one `#[inputs]` struct, without options or generic parameters, which the Orb
/// module can name.
#[test]
fn inputs_are_refused_when_ill_formed() {
    let service = "#[service] pub struct Ledger; impl Ledger { pub fn count() -> u32 { 0 } }";
    for (items, message) in [
        (
            "#[inputs] pub struct Lent { pub a: u32 } #[inputs] pub struct Borrowed { pub b: u32 }",
            "`Lent` and `Borrowed` are both marked #[inputs]; an Orb has one, the argument of `orb(..)`",
        ),
        (
            "#[inputs(shared)] pub struct Lent { pub a: u32 }",
            "#[inputs] takes no options",
        ),
        (
            "#[inputs] pub struct Lent<T> { pub a: T }",
            "`Lent` is the argument of `orb(..)`: it takes no generic parameter",
        ),
        (
            "mod host { #[inputs] struct Lent { pub a: u32 } }",
            "`Lent` is the inputs of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, whose `orb(..)` takes it",
        ),
    ] {
        let error = refused(PACKAGE, &format!("mod book {{ {items} {service} }}"));
        assert_eq!(error, message, "{items}");
    }
}

/// A service with a function marked `#[create]` is built by it, once per object: `orb(..)` shares
/// the inputs as one `Rc`, the registration takes them, and the factory calls the create with them
/// and the object's `Declaration`. What it returns is the service's environment: a method
/// borrowing `&self` is called on it, any other as before; a service without a create takes no
/// inputs. The receiver stays out of the WIT, and the component has no bridge for such a service,
/// whose objects only the native side builds.
#[test]
fn a_service_with_a_create_is_built_by_it() {
    let module = expanded(
        PACKAGE,
        "mod book {
            #[inputs]
            pub struct Inputs { pub base: u32 }
            #[service]
            pub struct Ledger { base: u32 }
            impl Ledger {
                #[create]
                fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                    Ok(Self { base: inputs.base })
                }
                pub fn count(&self) -> u32 { self.base }
                pub async fn total(&self, extra: u32) -> u32 { self.base + extra }
                pub fn zero() -> u32 { 0 }
            }
            #[service]
            pub struct Plain;
            impl Plain { pub fn one() -> u32 { 1 } }
        }",
    );
    let root = native(&module, &[]);
    for expected in [
        "pub fn orb (inputs : Inputs) -> :: tembra :: Result < :: tembra :: Orb > { orb_named (\"book\" , 1u32 , inputs) }",
        "pub fn orb_named (name : & str , api_version : u32 , inputs : Inputs) -> :: tembra :: Result < :: tembra :: Orb > { let inputs = :: std :: rc :: Rc :: new (inputs) ; :: core :: result :: Result :: Ok (:: tembra :: Orb :: new (name , api_version) . service (self :: __tembra_service_ledger (name , & inputs)) . service (self :: __tembra_service_plain (name))) }",
        "pub (crate) fn __tembra_service_ledger (orb : & str , inputs : & :: std :: rc :: Rc < Inputs >) -> :: tembra :: Service < Ledger > { const __TEMBRA_CODEC_COUNT :",
        "} ; let inputs = :: std :: rc :: Rc :: clone (inputs) ; :: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:book/ledger@1.0.0\") , move | declaration | Ledger :: create (& inputs , & declaration))",
        ". method_with (\"count\" , | __tembra_self : :: std :: rc :: Rc < Ledger > , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (__tembra_self . count ()) } , __TEMBRA_CODEC_COUNT ,)",
        ":: core :: result :: Result :: Ok (__tembra_self . total (extra) . await)",
        ". method_with (\"zero\" , | _ , __tembra_context : :: tembra :: Context , () : () | async move { __tembra_context . check () ? ; :: core :: result :: Result :: Ok (Ledger :: zero ()) } , __TEMBRA_CODEC_ZERO ,)",
        "pub (crate) fn __tembra_service_plain (orb : & str) -> :: tembra :: Service < () > { const __TEMBRA_CODEC_ONE :",
        "} ; :: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:book/plain@1.0.0\") , | _ | { :: core :: result :: Result :: Ok (()) })",
    ] {
        assert!(root.contains(expected), "{expected}\n{root}");
    }
    let derived = wit(&module);
    assert!(
        derived.contains("interface ledger {\n  count: func() -> u32;\n  total: async func(extra: u32) -> u32;\n  zero: func() -> u32;\n}"),
        "{derived}"
    );
    let tokens = tokens(&module);
    assert!(
        tokens.contains("impl exports :: example :: book :: plain :: Guest for __TembraExports"),
        "{tokens}"
    );
    assert!(!tokens.contains(":: ledger :: Guest for"), "{tokens}");
    // A service of a sub-module takes the inputs by their path from its module; a create that is
    // `pub` is no method.
    let module = expanded(
        PACKAGE,
        "mod book {
            #[inputs]
            pub struct Inputs { pub base: u32 }
            pub mod shelf {
                #[service]
                pub struct Shelf { base: u32 }
                impl Shelf {
                    #[create]
                    pub fn create(inputs: &super::Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                        Ok(Self { base: inputs.base })
                    }
                    pub fn count(&self) -> u32 { self.base }
                }
            }
        }",
    );
    let derived = wit(&module);
    assert!(
        derived.contains("interface shelf {\n  count: func() -> u32;\n}"),
        "{derived}"
    );
    let root = native(&module, &[]);
    assert!(
        root.contains(". service (self :: shelf :: __tembra_service_shelf (name , & inputs))"),
        "{root}"
    );
    let shelf = native(&module, &["shelf"]);
    assert!(
        shelf.contains(
            "pub (crate) fn __tembra_service_shelf (orb : & str , inputs : & :: std :: rc :: Rc < super :: Inputs >) -> :: tembra :: Service < Shelf >"
        ),
        "{shelf}"
    );
}

/// A `#[create]` is a function of a `#[service]` struct's impl block, one per service, that takes
/// the Orb's inputs and the object's `Declaration` by reference and returns the service; the Orb
/// has `#[inputs]`. A service with fields needs one, and so does a method borrowing `&self`, the
/// only way a method borrows the service.
#[test]
fn a_create_is_refused_when_ill_formed() {
    let inputs = "#[inputs] pub struct Inputs { pub base: u32 }";
    let create = "#[create] fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> { todo!() }";
    for (items, message) in [
        (
            format!(
                "#[service] pub struct Ledger; impl Ledger {{ {create} pub fn count() -> u32 {{ 0 }} }}"
            ),
            "`Ledger::create` is marked #[create], which takes the Orb's inputs: mark the struct `orb(..)` takes #[inputs]",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ #[create] async fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {{ todo!() }} pub fn count() -> u32 {{ 0 }} }}"
            ),
            "#[create] marks `fn(&Inputs, &Declaration) -> tembra::Result<Self>` of a #[service] struct",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ #[create] fn create(inputs: Inputs, declaration: &Declaration) -> tembra::Result<Self> {{ todo!() }} pub fn count() -> u32 {{ 0 }} }}"
            ),
            "#[create] marks `fn(&Inputs, &Declaration) -> tembra::Result<Self>` of a #[service] struct",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ #[create] fn create(&self, declaration: &Declaration) -> tembra::Result<Self> {{ todo!() }} pub fn count() -> u32 {{ 0 }} }}"
            ),
            "#[create] marks `fn(&Inputs, &Declaration) -> tembra::Result<Self>` of a #[service] struct",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ {create} {} pub fn count() -> u32 {{ 0 }} }}",
                create.replace("fn create", "fn build")
            ),
            "`Ledger` has two functions marked #[create]",
        ),
        (
            format!(
                "{inputs} pub struct Other; impl Other {{ {create} }} #[service] pub struct Ledger; impl Ledger {{ pub fn count() -> u32 {{ 0 }} }}"
            ),
            "#[create] marks a function of a #[service] struct; `Other` is not one",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger {{ base: u32 }} impl Ledger {{ pub fn count() -> u32 {{ 0 }} }}"
            ),
            "`Ledger` has fields, which a #[create] builds: give it a function marked #[create], `fn(&Inputs, &Declaration) -> tembra::Result<Self>`, or make it a unit struct",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ pub fn count(&self) -> u32 {{ 0 }} }}"
            ),
            "`Ledger::count` borrows `&self`, which a #[create] builds: give `Ledger` a function marked #[create], or remove self",
        ),
        (
            format!(
                "{inputs} #[service] pub struct Ledger; impl Ledger {{ {create} pub fn count(&mut self) -> u32 {{ 0 }} }}"
            ),
            "a method borrows `&self`; change the service through Cell or RefCell",
        ),
    ] {
        let error = refused(PACKAGE, &format!("mod book {{ {items} }}"));
        assert_eq!(error, message, "{items}");
    }
}

/// `#[service(key = main)]` and `#[service(principal = "..")]` guard the address of the service's
/// objects: the native factory refuses another key, or another principal, as forbidden with the
/// reason `key` or `principal`, before it builds the object, with its create when it has one. The
/// guards are no part of the WIT or of the manifest.
#[test]
fn an_address_guard_refuses_another_key_or_principal() {
    let key = "if address . key != [\"main\"] { return :: core :: result :: Result :: Err (:: tembra :: Error :: new (:: tembra :: Code :: Forbidden , \"key\" , \"the objects of example:book/ledger@1.0.0 use the key main\")) ; }";
    let principal = "if address . principal != \"host\" { return :: core :: result :: Result :: Err (:: tembra :: Error :: new (:: tembra :: Code :: Forbidden , \"principal\" , \"the objects of example:book/ledger@1.0.0 belong to the principal host\")) ; }";
    let module = expanded(PACKAGE, &ledger("#[service(key = main)]"));
    let generated = native(&module, &[]);
    assert!(
        generated.contains(&format!(
            ":: tembra :: Service :: new (:: std :: format ! (\"{{}}::{{}}\" , orb , \"example:book/ledger@1.0.0\") , | declaration | {{ let address = declaration . address () ? ; {key} :: core :: result :: Result :: Ok (()) }})"
        )),
        "{generated}"
    );
    assert!(!generated.contains("address . principal"), "{generated}");
    assert_eq!(wit(&module), wit(&expanded(PACKAGE, &ledger("#[service]"))));
    assert_eq!(manifest(&module), None);
    let module = expanded(PACKAGE, &ledger("#[service(principal = \"host\")]"));
    let generated = native(&module, &[]);
    assert!(
        generated.contains(&format!(
            "| declaration | {{ let address = declaration . address () ? ; {principal} :: core :: result :: Result :: Ok (()) }}"
        )),
        "{generated}"
    );
    assert!(!generated.contains("address . key"), "{generated}");
    // Both, with the ownership policy, and before a create.
    let module = expanded(
        PACKAGE,
        "mod book {
            #[inputs]
            pub struct Inputs { pub base: u32 }
            #[service(key = main, principal = \"host\", lifetime = host)]
            pub struct Ledger { base: u32 }
            impl Ledger {
                #[create]
                fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                    Ok(Self { base: inputs.base })
                }
                pub fn count(&self) -> u32 { self.base }
            }
        }",
    );
    let generated = native(&module, &[]);
    for expected in [
        format!(
            "move | declaration | {{ let address = declaration . address () ? ; {key} {principal} Ledger :: create (& inputs , & declaration) }}"
        ),
        ". lifetime (:: tembra :: core :: Lifetime :: Host , 0u32)".to_owned(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    let error = refused(PACKAGE, &ledger("#[service(key = owner)]"));
    assert_eq!(
        error,
        "the key guard is `key = main`: an object's key is then [\"main\"]"
    );
}

/// The host creates a host-lifetime object as `main` for the principal `host` when it installs the
/// Orb, so a guard naming another principal would refuse it there: the macro refuses the
/// combination, in either order, and keeps the guard of the principal `host`.
#[test]
fn a_host_lifetime_guards_no_other_principal() {
    let message = "host-lifetime objects belong to the principal host: the host creates them for itself when it installs the Orb, and `principal = \"alice\"` would refuse them";
    for attribute in [
        "#[service(lifetime = host, principal = \"alice\")]",
        "#[service(principal = \"alice\", key = main, lifetime = host)]",
    ] {
        assert_eq!(refused(PACKAGE, &ledger(attribute)), message, "{attribute}");
    }
    let module = expanded(
        PACKAGE,
        &ledger("#[service(lifetime = host, principal = \"host\")]"),
    );
    let generated = native(&module, &[]);
    assert!(
        generated.contains("if address . principal != \"host\""),
        "{generated}"
    );
}

/// The function marked `#[root]` is the Orb's create: `orb(..)` shares the inputs as one `Rc` and
/// hands `Orb::create` a closure calling the root with them and the Orb's `Declaration`. The root
/// stays the author's function, as written but for the marker, and its value no type of the WIT. In
/// a sub-module, the Orb module names it by its path.
#[test]
fn the_root_is_the_create_of_the_orb() {
    let service = "#[service] pub struct Ledger; impl Ledger { pub fn count() -> u32 { 0 } }";
    let module = expanded(
        PACKAGE,
        &format!(
            "mod book {{
                #[inputs]
                pub struct Inputs {{ pub base: u32 }}
                pub struct Shared {{ pub base: u32 }}
                #[root]
                fn shared(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Shared> {{
                    Ok(Shared {{ base: inputs.base }})
                }}
                {service}
            }}"
        ),
    );
    let root = native(&module, &[]);
    assert!(
        root.contains(
            "pub fn orb_named (name : & str , api_version : u32 , inputs : Inputs) -> :: tembra :: Result < :: tembra :: Orb > { let inputs = :: std :: rc :: Rc :: new (inputs) ; :: core :: result :: Result :: Ok (:: tembra :: Orb :: new (name , api_version) . create ({ let inputs = :: std :: rc :: Rc :: clone (& inputs) ; move | declaration | self :: shared (& inputs , & declaration) }) . service (self :: __tembra_service_ledger (name))) }"
        ),
        "{root}"
    );
    let tokens = tokens(&module);
    assert!(
        tokens.contains(
            "fn shared (inputs : & Inputs , declaration : & Declaration) -> tembra :: Result < Shared > { Ok (Shared { base : inputs . base }) }"
        ),
        "{tokens}"
    );
    assert!(!tokens.contains("root ]"), "{tokens}");
    assert_eq!(
        wit(&module),
        wit(&expanded(PACKAGE, &format!("mod book {{ {service} }}")))
    );
    let module = expanded(
        PACKAGE,
        &format!(
            "mod book {{
                #[inputs]
                pub struct Inputs {{ pub base: u32 }}
                pub mod shared {{
                    #[root]
                    pub(super) fn open(inputs: &super::Inputs, declaration: &Declaration) -> tembra::Result<u32> {{
                        Ok(inputs.base)
                    }}
                }}
                {service}
            }}"
        ),
    );
    let root = native(&module, &[]);
    assert!(
        root.contains(
            ". create ({ let inputs = :: std :: rc :: Rc :: clone (& inputs) ; move | declaration | self :: shared :: open (& inputs , & declaration) })"
        ),
        "{root}"
    );
}

/// A `#[root]` is a free function of the module, the Orb's one, without options, that takes the
/// Orb's inputs and its `Declaration` by reference and returns the root's value; the Orb has
/// `#[inputs]`, and the Orb module can call it.
#[test]
fn a_root_is_refused_when_ill_formed() {
    let inputs = "#[inputs] pub struct Inputs { pub base: u32 }";
    let root = "#[root] fn shared(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<u32> { todo!() }";
    let shape = "#[root] marks `fn(&Inputs, &Declaration) -> tembra::Result<T>`, a free function of the module";
    for (items, message) in [
        (
            root.to_owned(),
            "`shared` is marked #[root], which takes the Orb's inputs: mark the struct `orb(..)` takes #[inputs]",
        ),
        (
            format!("{inputs} {root} {}", root.replace("fn shared", "fn other")),
            "`shared` and `other` are both marked #[root]; an Orb has one, its create",
        ),
        (
            format!("{inputs} {}", root.replace("#[root]", "#[root(shared)]")),
            "#[root] takes no options",
        ),
        (
            format!("{inputs} {}", root.replace("fn shared", "async fn shared")),
            shape,
        ),
        (
            format!(
                "{inputs} {}",
                root.replace("inputs: &Inputs", "inputs: Inputs")
            ),
            shape,
        ),
        (
            format!("{inputs} {}", root.replace("inputs: &Inputs, ", "")),
            shape,
        ),
        (
            format!(
                "{inputs} {}",
                root.replace(" -> tembra::Result<u32> { todo!() }", " {}")
            ),
            shape,
        ),
        (
            format!("{inputs} mod shared {{ {root} }}"),
            "#[root] on a function of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, which calls it",
        ),
        (
            format!("{inputs} pub struct Shared; impl Shared {{ {root} }}"),
            "#[root] goes on a free function of the module, not in an impl block",
        ),
        (
            format!("{inputs} pub struct Shared; impl Clone for Shared {{ {root} }}"),
            "#[root] goes on a free function of the module, not in an impl block",
        ),
    ] {
        let error = refused(
            PACKAGE,
            &format!(
                "mod book {{ {items} #[service] pub struct Ledger; impl Ledger {{ pub fn count() -> u32 {{ 0 }} }} }}"
            ),
        );
        assert_eq!(error, message, "{items}");
    }
}

/// The analysis lists what makes a module native only, each marker as the module writes it, in
/// the order the scan finds them; a module a component could serve has none.
#[test]
fn the_analysis_lists_what_makes_a_module_native_only() {
    let markers = |module: &str| {
        let module: ItemMod = syn::parse_str(module).unwrap();
        let analysis = super::analyze(options(PACKAGE, None), module)
            .unwrap_or_else(|error| panic!("{error}"));
        analysis
            .native_only
            .iter()
            .map(|marker| marker.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        markers(
            "mod book {
                #[inputs]
                pub struct Inputs { pub base: u32 }
                #[root]
                fn shared(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<u32> { Ok(0) }
                #[service(key = main, principal = \"host\")]
                pub struct Ledger { base: u32 }
                impl Ledger {
                    #[create]
                    fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                        Ok(Self { base: inputs.base })
                    }
                    pub fn count(&self) -> u32 { self.base }
                }
            }"
        ),
        [
            "#[inputs]",
            "#[root]",
            "key = main",
            "principal = \"host\"",
            "#[create]"
        ]
    );
    assert!(markers(&ledger("#[service]")).is_empty());
}

/// A native-only module may export nothing when what it runs is its root or its root resources:
/// the WIT is then an empty world, and `orb(..)` an Orb of no service whose create is the root, or
/// declares the resources. A module that is not native only, or has nothing to run, still exports
/// something.
#[test]
fn a_native_only_module_may_only_run_its_root() {
    const WORKER: &str = "example:worker@1.0.0";
    let inputs = "#[inputs] pub struct Inputs { pub label: String }";
    let counts = "#[resource] pub struct Counts { pub seen: u32 } impl Counts { #[open] pub fn open() -> Counts { Counts { seen: 0 } } }";
    let module = expanded(
        WORKER,
        &format!(
            "mod worker {{
                {inputs}
                #[root]
                fn start(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<()> {{ Ok(()) }}
            }}"
        ),
    );
    let derived = wit(&module);
    assert!(derived.contains("world worker {\n}"), "{derived}");
    assert!(!derived.contains("interface"), "{derived}");
    let root = native(&module, &[]);
    assert!(
        root.contains(
            ":: core :: result :: Result :: Ok (:: tembra :: Orb :: new (name , api_version) . create ({ let inputs = :: std :: rc :: Rc :: clone (& inputs) ; move | declaration | self :: start (& inputs , & declaration) })) }"
        ),
        "{root}"
    );
    let module = expanded(WORKER, &format!("mod worker {{ {inputs} {counts} }}"));
    let derived = wit(&module);
    assert!(
        derived.contains("world worker {\n  import tembra:guest/resources@1.0.0;\n  export tembra:guest/resources-init@1.0.0;\n}"),
        "{derived}"
    );
    let root = native(&module, &[]);
    assert!(
        root.contains(
            ":: core :: result :: Result :: Ok (:: tembra :: Orb :: new (name , api_version) . create ({ let resources = :: std :: rc :: Rc :: clone (& resources) ; move | declaration | resources . declare (& declaration) })) }"
        ),
        "{root}"
    );
    assert_eq!(
        refused(WORKER, &format!("mod worker {{ {counts} }}")),
        "nothing to export: mark a unit struct with #[service] and give it an impl block of pub fn, or mark a hook with #[hook]"
    );
    assert_eq!(
        refused(WORKER, &format!("mod worker {{ {inputs} }}")),
        "nothing to export or run: mark a unit struct with #[service] and give it an impl block of pub fn, mark a hook with #[hook], or give the Orb a function marked #[root] or a #[resource]"
    );
}

/// A service of a native-only module may have no method when its create builds its objects, which
/// then only run what the create declares while they are referenced. Its interface is empty in the
/// WIT, and a contract plans no service for an empty interface, so the service has no signature:
/// its registration is a factory, without a signature or a method. A component's service, or one
/// without a create, still has a method.
#[test]
fn a_native_only_service_may_have_no_method() {
    let porter = "#[service] pub struct Porter; impl Porter {
        #[create]
        fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> { Ok(Self) }
    }";
    let module = expanded(
        PACKAGE,
        &format!("mod book {{ #[inputs] pub struct Inputs {{ pub base: u32 }} {porter} }}"),
    );
    let derived = wit(&module);
    assert!(derived.contains("interface porter {\n}"), "{derived}");
    assert!(derived.contains("  export porter;\n"), "{derived}");
    let root = native(&module, &[]);
    assert!(
        root.contains(
            ":: tembra :: Service :: new (:: std :: format ! (\"{}::{}\" , orb , \"example:book/porter@1.0.0\") , move | declaration | Porter :: create (& inputs , & declaration)) }"
        ),
        "{root}"
    );
    assert!(!root.contains("method_with"), "{root}");
    assert!(
        !tokens(&module).contains("SIGNATURE"),
        "{}",
        tokens(&module)
    );
    assert_eq!(
        refused(
            PACKAGE,
            "mod book { #[service] pub struct Ledger; impl Ledger {} }"
        ),
        "a service needs at least one pub fn in an impl block"
    );
    // Without a create, its objects would run nothing.
    assert_eq!(
        refused(
            PACKAGE,
            "mod book { #[inputs] pub struct Inputs { pub base: u32 } #[service] pub struct Ledger; impl Ledger {} }"
        ),
        "a service needs at least one pub fn in an impl block, or a function marked #[create] that declares what its objects run"
    );
}

/// The contract an Orb crate carries, written as `cargo tembra build` writes it: only when the
/// macro is given the crate's directory, which `primary_crate_dir` takes from Cargo's variables
/// as `expansion` reads them, so a dependency's is never written and the token tests write nothing.
#[test]
fn the_contract_is_written_into_the_crate_compiled_for_itself_alone() {
    let dir =
        std::env::temp_dir().join(format!("tembra-orb-macro-contract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let module = || syn::parse_str::<ItemMod>(COUNTER).unwrap();
    let manifest_dir = || Some(dir.clone().into_os_string());
    // A dependency: Cargo sets no CARGO_PRIMARY_PACKAGE, and nothing is written.
    let dependency = primary_crate_dir(None, manifest_dir());
    assert_eq!(dependency, None);
    expand(
        options("example:counter@1.0.0", None),
        module(),
        Metadata::named("counter"),
        dependency.as_deref(),
    )
    .unwrap();
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    // The crate compiled for itself: Cargo sets CARGO_PRIMARY_PACKAGE to 1.
    let primary = primary_crate_dir(Some("1".into()), manifest_dir());
    assert_eq!(primary.as_deref(), Some(dir.as_path()));
    let expanded: ItemMod = syn::parse2(
        expand(
            options("example:counter@1.0.0", None),
            module(),
            Metadata::named("counter"),
            primary.as_deref(),
        )
        .unwrap(),
    )
    .unwrap();
    let contract = dir.join("contract");
    assert_eq!(
        std::fs::read_to_string(contract.join("tally.wit")).unwrap(),
        wit(&expanded)
    );
    // The state imports the kernel's package, which goes to deps/, and the directory reads back
    // as the contract, as a consumer's `wit = "../counter/contract"` reads it.
    let deps: Vec<String> = std::fs::read_dir(contract.join("deps"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(deps, ["tembra-guest.wit"]);
    tembra_contracts::Contract::from_dir(&contract, Some("tally")).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

/// A module that reads its consumed directory from every place the expansion reads it: an
/// implemented interface and an imported one, a hook it contributes to, an interface imported by
/// key, a type of an import in a derived signature and in a state, whose serde derives make the
/// expansion read the WIT as the bindings do, and a WASI import, which nests the bundled packages.
#[test]
fn an_expansion_reads_the_consumed_directory_and_the_bundled_packages_once() {
    let dir = consumed(
        "once",
        "package example:ledger@1.0.0;

interface book {
  record entry { label: string, amount: s64 }
  add: func(entry: entry) -> u32;
}

interface guard {
  record admission { value: s32, blocked: bool }
  apply: func(input: admission) -> admission;
}

interface reports {
  record report { total: s64 }
  latest: func() -> report;
}

interface counter {
  add: func(amount: u32) -> u32;
}
",
    );
    let options = Options {
        imports: vec![
            literal("example:ledger/reports@1.0.0"),
            literal("wasi:random/random@0.3.0"),
        ],
        keyed: vec![literal("example:ledger/counter@1.0.0")],
        ..options("example:keeper@1.0.0", Some(&dir))
    };
    let directories = super::wit::DIRECTORY_PARSES.get();
    let bundled = super::wasi::BUNDLED_PARSES.get();
    let module = expanded_with(
        options,
        "mod keeper {
            #[state(service = Keeper)]
            pub struct Kept { pub total: i64, pub last: Option<reports::Report> }
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper { pub fn add(entry: book::Entry) -> u32 { 0 } }
            #[hook(\"example:ledger/guard@1.0.0\", priority = 1)]
            pub fn admit(input: guard::Admission) -> guard::Admission { input }
            #[service]
            pub struct Report;
            impl Report {
                pub fn latest() -> reports::Report { reports::latest() }
                pub async fn bump(key: String) -> Result<u32, String> {
                    Ok(counter_ref::get(vec![key]).await?.add(1))
                }
                pub fn draw(length: u64) -> Vec<u8> { random::get_random_bytes(length) }
            }
        }",
    );
    let text = wit(&module);
    for expected in [
        "  use example:ledger/reports@1.0.0.{report};\n",
        "  import example:ledger/reports@1.0.0;\n",
        "  import wasi:random/random@0.3.0;\n",
        "  import counter-ref;\n",
        "  import example:ledger/guard@1.0.0;\n  export example:ledger/guard@1.0.0;\n",
        "  export example:ledger/book@1.0.0;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(
        tokens(&module)
            .contains("additional_derives : [:: serde :: Serialize , :: serde :: Deserialize]")
    );
    assert_eq!(
        (
            super::wit::DIRECTORY_PARSES.get() - directories,
            super::wasi::BUNDLED_PARSES.get() - bundled,
        ),
        (1, 1),
        "the consumed directory and the bundled WASI packages are read once each"
    );
}

/// A module whose expansion reads the bundled WASI packages in every place that reads them: a WASI
/// import, which nests the packages it needs in the inline WIT, a state holding a WASI record,
/// which the expansion looks up in the bundled packages, and the state's serde derives, which make
/// the expansion read the WIT as the bindings do, the packages the inline WIT nests included. A
/// test build counts a parse of the bundled packages wherever the expansion parses them, the WIT the
/// bindings read included.
#[test]
fn every_reader_of_the_bundled_packages_takes_them_parsed_once() {
    let options = Options {
        imports: vec![literal("wasi:clocks/system-clock@0.3.0")],
        ..options("example:watch@1.0.0", None)
    };
    let bundled = super::wasi::BUNDLED_PARSES.get();
    let module = expanded_with(
        options,
        "mod watch {
            #[state(service = Clock)]
            pub struct Seen { pub last: Option<system_clock::Instant> }
            #[service]
            pub struct Clock;
            impl Clock {
                pub fn seconds() -> i64 { system_clock::now().seconds }
            }
        }",
    );
    assert!(
        tokens(&module)
            .contains("additional_derives : [:: serde :: Serialize , :: serde :: Deserialize]")
    );
    assert_eq!(
        super::wasi::BUNDLED_PARSES.get() - bundled,
        1,
        "the bundled WASI packages are parsed once"
    );
}
