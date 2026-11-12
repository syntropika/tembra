//! The macro's reading of a module, checked on its output: the WIT it derives, the manifest it
//! embeds and the errors it reports. The generated bindings themselves compile in the examples.
use super::{Options, expand, expansion};
use syn::{Expr, Item, ItemMod, Lit, UnOp};

/// Expand a module under `#[orb(package = ...)]` and parse the generated module back.
fn orb(package: &str, module: &str) -> syn::Result<ItemMod> {
    orb_with(options(package, None), module)
}

/// A string literal, as written on the attribute.
fn literal(text: &str) -> syn::LitStr {
    syn::LitStr::new(text, proc_macro2::Span::call_site())
}

fn options(package: &str, wit: Option<&std::path::Path>) -> Options {
    Options {
        package: Some(literal(package)),
        wit: wit.map(|dir| dir.display().to_string()),
        context: false,
        no_std: false,
        imports: Vec::new(),
    }
}

/// A directory holding one consumed WIT package, under the system's temporary directory.
fn consumed(name: &str, wit: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tembra-orb-macro-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.wit"), wit).unwrap();
    dir
}

/// The generated module as one token string, for looking up generated items.
fn tokens(module: &ItemMod) -> String {
    quote::quote!(#module).to_string()
}

fn orb_with(options: Options, module: &str) -> syn::Result<ItemMod> {
    let module: ItemMod = syn::parse_str(module).expect("a module");
    syn::parse2(expand(options, module)?)
}

fn expanded(package: &str, module: &str) -> ItemMod {
    orb(package, module).unwrap_or_else(|error| panic!("{error}"))
}

fn refused(package: &str, module: &str) -> String {
    refused_with(options(package, None), module)
}

fn refused_with(options: Options, module: &str) -> String {
    match orb_with(options, module) {
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

/// The JSON of the `tembra:manifest` section, if the module embeds one.
fn manifest(module: &ItemMod) -> Option<String> {
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

/// `context` on the attribute imports the kernel's context interface, alone, and re-exports its
/// generated module under its own name.
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
    assert!(
        items(&module)
            .iter()
            .any(|item| matches!(item, Item::Use(reexport)
        if quote::quote!(#reexport).to_string().contains("tembra :: guest :: context")))
    );
    let plain = expanded(PACKAGE, &ledger("#[service]"));
    assert!(!wit(&plain).contains("context"));
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
/// their visibility, so the bridge calls them, never the trait's function of the same name.
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
        "extern crate alloc as __tembra_alloc ; } } struct __TembraExports ;",
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
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str("orb.wit", &text)
        .unwrap_or_else(|error| panic!("{error:#}\n{text}"));
    text
}

/// An interface `use`s what the constructors, methods and static functions of its own resources
/// name, as it does for its own functions: a module record, another service's resource, a type a
/// derived hook defines.
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
    let resource = resolved(&expanded(
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
    ));
    assert!(
        resource.contains("interface sessions {\n  use main.{token};\n"),
        "{resource}"
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
/// functions, its resources' functions or other types, and re-exported from there; so a first
/// service handing out a later service's resource does not make the later one `use` it back.
/// Interfaces that would still `use` each other are an error naming them.
#[test]
fn a_type_is_defined_by_the_earliest_service_that_mentions_it() {
    let module = expanded("example:wb@1.0.0", FRONT);
    assert_eq!(
        resolved(&module),
        "package example:wb@1.0.0;\n\ninterface front {\n  use texts.{builder};\n  start: func() -> builder;\n}\n\ninterface texts {\n  /// Counts over the texts.\n  record stats {\n    words: u32,\n  }\n  resource builder {\n    text: func() -> u32;\n  }\n  counts: func() -> stats;\n}\n\nworld wb {\n  export front;\n  export texts;\n}\n"
    );
    assert!(
        tokens(&module).contains("pub use exports :: example :: wb :: texts :: { Stats } ;"),
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
    // Both services mention the record and the first hands out the second's resource: each
    // interface would `use` the other.
    let error = refused(
        "example:wb@1.0.0",
        &FRONT.replace(
            "impl Front { pub fn start() -> Builder { Builder } }",
            "impl Front { pub fn start() -> Builder { Builder } pub fn summary() -> Stats { Stats { words: 0 } } }",
        ),
    );
    assert!(
        error.contains("`front` uses `{builder}` of `texts`, `texts` uses `{stats}` of `front`: WIT refuses interfaces that depend on themselves through `use`"),
        "{error}"
    );
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
    let error = refused(
        "example:desk@1.0.0",
        &DESK.replace(
            "pub fn open(n: u32) -> Session { Session::new(n) }",
            "pub fn open(n: u32) -> Session { Session::new(n) } pub fn take(session: Session) {}",
        ),
    );
    assert!(error.contains("handed out, never taken"), "{error}");
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

/// A type is translated to WIT only once a method, a resource or a hook reaches it: a type no
/// method mentions stays ordinary Rust whatever it holds, an array, a reference or a variant with
/// named fields, and the same type is a compile error once it enters the WIT.
#[test]
fn only_a_type_that_enters_the_wit_needs_a_wit_spelling() {
    let module = |returned: &str| {
        format!(
            "mod safe {{
                pub struct Digest {{ pub bytes: [u8; 32] }}
                pub enum Shape {{ Circle {{ radius: u32 }}, Square(u32) }}
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
        format!("{derived} pub enum Shape {{ Circle {{ radius : u32 }} , Square (u32) }}"),
        "pub struct Banner { pub text : & 'static str }".into(),
    ] {
        assert!(generated.contains(&expected), "{expected}\n{generated}");
    }
    for (returned, message) in [
        ("Digest", "this type has no WIT equivalent"),
        ("Shape", "a variant carries at most one unnamed payload"),
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

/// A state starts from its `Default`: the macro implements it field by field unless the author
/// gives it, by an `impl Default` anywhere in the Orb module or by a derive on a state the macro
/// keeps, out of the WIT; the serde derives the author already wrote are not added again.
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
    // A state that enters the WIT is the generated type: the author's derive goes with the item,
    // so the macro implements `Default`, as the derive would have.
    let entered = tokens(&expanded(
        "example:ledger@1.0.0",
        &state("#[derive(Default)]", "", "Balance"),
    ));
    assert!(entered.contains(generated_default), "{entered}");
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
/// left out of its inline WIT, whatever the copy's comments say; a copy defining the package
/// otherwise is an error naming its file.
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
    let commented = GUEST
        .replace("/// ", "/// (another text) ")
        .replace('\n', "\r\n");
    for (name, guest) in [
        ("first-deps", GUEST),
        ("first-comments", commented.as_str()),
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
    let error = with_deps(
        "first-different",
        &GUEST.replacen("read: func() -> string;", "read: func() -> u32;", 1),
    )
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("deps/tembra-guest.wit:")
            && error.contains("this copy of tembra:guest@1.0.0, the kernel's package, defines it otherwise than the one bundled with this macro"),
        "{error}"
    );
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
