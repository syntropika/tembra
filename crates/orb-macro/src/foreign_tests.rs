//! The types of the consumed interfaces in derived signatures: the `use` lines the derived WIT
//! gains, the bridges that pass the author's values as they are, the states that hold them, and
//! what is refused where.
use super::{Options, expand};
use std::path::{Path, PathBuf};
use syn::{Expr, Item, ItemMod, Lit};

/// A consumed package whose `catalog` interface has a type of every kind, one it uses from another
/// interface, two aliases, a WIT name the bindings spell otherwise, a resource and records holding
/// a handle and a flags.
const SHOP: &str = "package example:shop@1.0.0;

interface kinds {
  enum status { open, closed }
}

/// What the shop sells.
interface catalog {
  use kinds.{status};
  /// One item of the catalog.
  record item { name: string, price: u32, status: status }
  variant pick { item(item), name(string), none }
  enum size { small, large }
  flags marks { seen, kept }
  type items = list<item>;
  type label = item;
  record a-b { x: u32 }
  resource cart {
    constructor();
    add: func(item: item) -> u32;
  }
  record slot { cart: cart }
  record tagged { marks: marks }
  lookup: func(name: string) -> option<item>;
}
";

fn literal(text: &str) -> syn::LitStr {
    syn::LitStr::new(text, proc_macro2::Span::call_site())
}

/// A directory holding one consumed WIT package, under the system's temporary directory.
fn consumed(name: &str, wit: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tembra-orb-macro-foreign-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("package.wit"), wit).unwrap();
    dir
}

/// The options of `package`, consuming `dir` and importing `imports`.
fn options(package: &str, dir: Option<&Path>, imports: &[&str]) -> Options {
    Options {
        package: Some(literal(package)),
        wit: dir.map(|dir| dir.display().to_string()),
        imports: imports.iter().map(|identity| literal(identity)).collect(),
        ..Options::default()
    }
}

/// The options of an Orb importing the shop's catalog from `dir`.
fn shopping(dir: &Path) -> Options {
    options(
        "example:shopper@1.0.0",
        Some(dir),
        &["example:shop/catalog@1.0.0"],
    )
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

/// The derived WIT, from the generated `WIT` constant.
fn wit(module: &ItemMod) -> String {
    module
        .content
        .as_ref()
        .expect("a body")
        .1
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

/// The derived WIT, after checking that it resolves next to the consumed packages of `dir` and the
/// kernel's package a state imports from.
fn resolved(module: &ItemMod, dir: &Path) -> String {
    let text = wit(module);
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str("guest.wit", tembra_contracts::guest::SOURCE)
        .unwrap();
    resolve.push_dir(dir).unwrap();
    resolve
        .push_str("orb.wit", &text)
        .unwrap_or_else(|error| panic!("{error:#}\n{text}"));
    text
}

fn tokens(module: &ItemMod) -> String {
    quote::quote!(#module).to_string()
}

const SHOPPER: &str = "mod shopper {
    /// What a basket holds.
    pub struct Basket { pub items: Vec<catalog::Item>, pub size: catalog::Size, pub marks: catalog::Marks }
    /// What a shopper chose.
    pub enum Choice { Picked(catalog::Pick), Nothing }
    #[wit_resource]
    pub struct Till { total: core::cell::Cell<u32> }
    impl Till {
        #[constructor]
        pub fn new(first: catalog::Item) -> Self { Till { total: core::cell::Cell::new(first.price) } }
        pub fn ring(&self, item: catalog::Label) -> catalog::Status { catalog::Status::Open }
    }
    #[service]
    pub struct Shopping;
    impl Shopping {
        pub fn price(item: catalog::Item) -> u32 { item.price }
        pub fn cheapest(items: catalog::Items, status: Option<catalog::Status>) -> Option<catalog::Item> { None }
        pub fn fill(items: Vec<catalog::Item>, size: catalog::Size, marks: catalog::Marks) -> Basket { Basket { items, size, marks } }
        pub fn choose(picks: (catalog::Pick, u32), odd: catalog::AB) -> Result<Choice, String> { todo!() }
        pub fn open(first: catalog::Item) -> Till { Till::new(first) }
    }
}";

/// A derived signature names an imported interface's types through the interface's module,
/// alone, in containers, as a resource's constructor and method take them and in the fields and
/// cases of the module's types: the derived interface `use`s each, under the kebab-case name of
/// its Rust name, and the bridges pass the author's values as they are, the bindings' own types.
#[test]
fn a_derived_signature_names_an_imported_interfaces_types() {
    let dir = consumed("shopper", SHOP);
    let module = expanded(shopping(&dir), SHOPPER);
    let text = resolved(&module, &dir);
    for expected in [
        "interface shopping {\n  use example:shop/catalog@1.0.0.{item, items, status, size, marks, pick, a-b as ab, label};\n",
        "  record basket {\n    items: list<item>,\n    size: size,\n    marks: marks,\n  }\n",
        "  variant choice {\n    picked(pick),\n    nothing,\n  }\n",
        "    constructor(first: item);\n",
        "    ring: func(item: label) -> status;\n",
        "  price: func(item: item) -> u32;\n",
        "  cheapest: func(items: items, status: option<status>) -> option<item>;\n",
        "  choose: func(picks: tuple<pick, u32>, odd: ab) -> result<choice, string>;\n",
        "world shopper {\n  import example:shop/catalog@1.0.0;\n  export shopping;\n}\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    for expected in [
        "fn price (item : catalog :: Item) -> u32 { Shopping :: price (item) }",
        "fn cheapest (items : catalog :: Items , status : Option < catalog :: Status >) -> Option < catalog :: Item > { Shopping :: cheapest (items , status) }",
        "fn choose (picks : (catalog :: Pick , u32) , odd : catalog :: AB) -> Result < Choice , String > { Shopping :: choose (picks , odd) }",
        "fn new (first : catalog :: Item) -> Self { Till :: new (first) }",
        "fn ring (& self , item : catalog :: Label) -> catalog :: Status { Till :: ring (self , item) }",
        "pub use exports :: example :: shopper :: shopping :: { Basket , Choice } ;",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// A type of the module that holds a consumed type is defined in one interface, which `use`s the
/// consumed type; another interface that names the type `use`s it from there, and nothing from
/// the consumed interface.
#[test]
fn the_interface_defining_a_type_uses_what_its_fields_name() {
    let dir = consumed("defining", SHOP);
    let module = expanded(
        shopping(&dir),
        "mod shopper {
            pub struct Basket { pub items: Vec<catalog::Item> }
            #[service]
            pub struct Filling;
            impl Filling { pub fn fill(count: u32) -> Basket { todo!() } }
            #[service]
            pub struct Paying;
            impl Paying { pub fn pay(basket: Basket) -> u32 { 0 } }
        }",
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface filling {\n  use example:shop/catalog@1.0.0.{item};\n  record basket {\n    items: list<item>,\n  }\n",
        "interface paying {\n  use filling.{basket};\n  pay: func(basket: basket) -> u32;\n}\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

/// The greeter's greeting, imported by an Orb with a sub-module of the same name.
const GREETER: &str = "package example:greeter@1.0.0;

interface greeting {
  record entry { text: string }
  greet: func(entry: entry) -> string;
}
";

/// A path resolves in the module that writes it before the consumed interfaces: `greeting::Banner`
/// written next to the Orb's own `mod greeting` is that module's type, whether or not the imported
/// `greeting` defines a `banner`, while `greeting::Entry` written where `greeting` is the imported
/// interface's module, and `super::greeting::Entry` from a sub-module, is the imported record.
#[test]
fn a_path_resolves_in_the_module_that_writes_it_before_the_consumed_interfaces() {
    const ARCHIVIST: &str = "mod archivist {
        pub mod front {
            pub mod greeting {
                pub struct Banner { pub text: String }
            }
            #[service]
            pub struct Front;
            impl Front {
                pub fn headline() -> greeting::Banner { greeting::Banner { text: String::new() } }
                pub fn keep(entry: super::greeting::Entry) -> u32 { 0 }
            }
        }
        #[service]
        pub struct Archive;
        impl Archive {
            pub fn latest(entries: Vec<greeting::Entry>) -> Option<greeting::Entry> { None }
        }
    }";
    let with_banner = GREETER.replace(
        "record entry",
        "record banner { text: string }\n  record entry",
    );
    for (name, greeter) in [("front", GREETER), ("front-banner", with_banner.as_str())] {
        let dir = consumed(name, greeter);
        let module = expanded(
            options(
                "example:archivist@1.0.0",
                Some(&dir),
                &["example:greeter/greeting@1.0.0"],
            ),
            ARCHIVIST,
        );
        let text = resolved(&module, &dir);
        for expected in [
            "interface front {\n  use example:greeter/greeting@1.0.0.{entry};\n  record banner {\n    text: string,\n  }\n  headline: func() -> banner;\n  keep: func(entry: entry) -> u32;\n}\n",
            "interface archive {\n  use example:greeter/greeting@1.0.0.{entry};\n  latest: func(entries: list<entry>) -> option<entry>;\n}\n",
        ] {
            assert!(text.contains(expected), "{name}: {expected}\n{text}");
        }
        assert!(!text.contains("{entry, banner}"), "{name}: {text}");
        let generated = tokens(&module);
        for expected in [
            "pub mod greeting { pub use super :: super :: exports :: example :: archivist :: front :: { Banner } ; }",
            "fn headline () -> greeting :: Banner { Front :: headline () }",
            "fn keep (entry : super :: greeting :: Entry) -> u32 { Front :: keep (entry) }",
            "fn latest (entries : Vec < greeting :: Entry >) -> Option < greeting :: Entry > { Archive :: latest (entries) }",
        ] {
            assert!(
                generated.contains(expected),
                "{name}: {expected}\n{generated}"
            );
        }
    }
}

/// The interface a module imports by key, whose view `use`s its record.
const COUNTERS: &str = "package example:counters@1.0.0;

interface counter {
  record tally { key: list<string>, count: u32 }
  add: func(amount: u32) -> u32;
  read: async func() -> tally;
}
";

/// A type the view of a keyed import re-exports is the interface's own: a derived signature names
/// it through the view's module, `counter_ref::Tally`, and the derived interface `use`s it from
/// the interface; the view's objects stay with the call that acquired them.
#[test]
fn a_derived_signature_names_a_keyed_imports_types_through_its_view() {
    let dir = consumed("router", COUNTERS);
    let keyed = || Options {
        keyed: vec![literal("example:counters/counter@1.0.0")],
        ..options("example:router@1.0.0", Some(&dir), &[])
    };
    let routing = |functions: &str| {
        format!(
            "mod router {{
                #[service]
                pub struct Routing;
                impl Routing {{ {functions} }}
            }}"
        )
    };
    let module = expanded(
        keyed(),
        &routing(
            "pub async fn read(key: String) -> Result<counter_ref::Tally, String> {
                Ok(counter_ref::get(vec![key]).await?.read().await)
            }
            pub fn count(tallies: Vec<counter_ref::Tally>) -> u32 { 0 }",
        ),
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface routing {\n  use example:counters/counter@1.0.0.{tally};\n",
        "  read: async func(key: string) -> result<tally, string>;\n",
        "  count: func(tallies: list<tally>) -> u32;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(
        tokens(&module).contains(
            "fn count (tallies : Vec < counter_ref :: Tally >) -> u32 { Routing :: count (tallies) }"
        ),
        "{}",
        tokens(&module)
    );
    let error = refused(
        keyed(),
        &routing("pub fn keep(counter: counter_ref::Counter) -> u32 { 0 }"),
    );
    assert!(
        error.contains("`Routing::keep` takes `Counter`, an object of the interface example:counters/counter@1.0.0, which this module imports by key: such an object belongs to the call that acquired it, so a derived interface takes and returns none"),
        "{error}"
    );
}

/// The ledger's book, implemented; a hook a module contributes to; and a hook a module declares.
const LEDGER: &str = "package example:ledger@1.0.0;

interface book {
  record entry { label: string, amount: s64 }
  resource cursor {
    constructor();
  }
  add: func(entry: entry) -> u32;
}

interface guard {
  record admission { value: s32, blocked: bool }
  apply: func(input: admission) -> admission;
}

interface gate {
  record ticket { value: s32 }
  apply: func(input: ticket) -> ticket;
}
";

fn keeper(report: &str) -> String {
    format!(
        "mod keeper {{
            #[service(\"example:ledger/book@1.0.0\")]
            pub struct Keeper;
            impl Keeper {{ pub fn add(entry: book::Entry) -> u32 {{ 0 }} }}
            #[wit_resource(service = Keeper)]
            pub struct Cursor;
            impl Cursor {{
                #[constructor]
                pub fn new() -> Self {{ Cursor }}
            }}
            #[hook(\"example:ledger/guard@1.0.0\", priority = 1)]
            pub fn admit(input: guard::Admission) -> guard::Admission {{ input }}
            #[service]
            pub struct Report;
            impl Report {{ {report} }}
        }}"
    )
}

/// An implemented interface and a hook the module contributes to are exported by the world, and a
/// derived interface takes their types from the export: the modules the Orb module re-exports for
/// them, so a derived signature names `book::Entry` and `guard::Admission`, and their resources
/// stay theirs.
#[test]
fn a_derived_signature_names_an_implemented_interfaces_and_a_hooks_types() {
    let dir = consumed("keeper", LEDGER);
    let module = expanded(
        options("example:keeper@1.0.0", Some(&dir), &[]),
        &keeper(
            "pub fn last(entries: Vec<book::Entry>) -> Option<book::Entry> { None }
            pub fn judged(admission: guard::Admission) -> bool { admission.blocked }",
        ),
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface report {\n  use example:ledger/book@1.0.0.{entry};\n  use example:ledger/guard@1.0.0.{admission};\n",
        "  last: func(entries: list<entry>) -> option<entry>;\n",
        "  judged: func(admission: admission) -> bool;\n",
        "  import example:ledger/guard@1.0.0;\n  export example:ledger/guard@1.0.0;\n  export example:ledger/book@1.0.0;\n  export report;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    for expected in [
        "fn last (entries : Vec < book :: Entry >) -> Option < book :: Entry > { Report :: last (entries) }",
        "fn judged (admission : guard :: Admission) -> bool { Report :: judged (admission) }",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    let refused = |report: &str| {
        refused(
            options("example:keeper@1.0.0", Some(&dir), &[]),
            &keeper(report),
        )
    };
    let error = refused("pub fn take(cursor: book::Cursor) -> u32 { 0 }");
    assert!(
        error.contains("`Report::take` takes `Cursor`, a resource of the implemented interface example:ledger/book@1.0.0: a derived function takes the resources of the interfaces this module derives and the handles of the interfaces it imports"),
        "{error}"
    );
    let error = refused("pub fn give() -> Option<book::Cursor> { None }");
    assert!(
        error.contains("`Report::give` returns `Cursor`, a resource of the implemented interface example:ledger/book@1.0.0: the runtime returns a resource only from the interface that defines it"),
        "{error}"
    );
}

/// The world exports a hook the module declares from a consumed package, which a derived
/// interface takes the types of, while the Orb module re-exports the hook's import, whose types
/// `run` takes and returns: a derived signature names none of them. A state holds them.
#[test]
fn a_declared_hooks_types_stay_out_of_a_derived_signature() {
    let dir = consumed("gatekeeper", LEDGER);
    let module = |admit: &str| {
        format!(
            "mod gatekeeper {{
                #[hook(\"example:ledger/gate@1.0.0\", composition = waterfall)]
                pub struct Gate;
                #[state]
                pub struct Last {{ pub ticket: Option<gate::Ticket> }}
                #[service]
                pub struct Admissions;
                impl Admissions {{ {admit} }}
            }}"
        )
    };
    let error = refused(
        options("example:gatekeeper@1.0.0", Some(&dir), &[]),
        &module("pub fn admit(ticket: gate::Ticket) -> u32 { 0 }"),
    );
    assert!(
        error.contains("`Admissions::admit` takes `Ticket`, a record of the hook example:ledger/gate@1.0.0, which this module declares: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `gate::Ticket` and `Gate::run` are the import's; use a type of the module"),
        "{error}"
    );
    let module = expanded(
        options("example:gatekeeper@1.0.0", Some(&dir), &[]),
        &module(
            "pub fn admit(value: i32) -> i32 {
                let ticket = Gate::run(gate::Ticket { value });
                Last::update(|last| last.ticket = Some(ticket));
                value
            }",
        ),
    );
    assert!(
        tokens(&module).contains("pub struct Last { pub ticket : Option < gate :: Ticket > }"),
        "{}",
        tokens(&module)
    );
}

/// What a derived interface does not take from a consumed interface is refused where it is named:
/// a name the interface does not define, a WASI type, a consumed type holding a resource handle,
/// an imported handle in a type of the module, a consumed type in a derived hook's input, and a
/// type of the module named like a consumed type in the interface that knows both.
#[test]
fn what_a_derived_interface_cannot_take_from_a_consumed_interface_is_refused() {
    let dir = consumed("refused", SHOP);
    let service = |functions: &str, items: &str| {
        format!(
            "mod shopper {{
                {items}
                #[service]
                pub struct Shopping;
                impl Shopping {{ {functions} }}
            }}"
        )
    };
    for (functions, items, message) in [
        (
            "pub fn price(item: catalog::Itme) -> u32 { 0 }",
            "",
            "`Shopping::price` takes `Itme`, which names no type or resource of the imported interface example:shop/catalog@1.0.0: its types are `Status`, `Item`, `Pick`, `Size`, `Marks`, `Items`, `Label`, `AB`, `Slot`, `Tagged`; its resources are `Cart`",
        ),
        (
            "pub fn hold(slots: Vec<catalog::Slot>) -> u32 { 0 }",
            "",
            "`Shopping::hold` takes `Slot`, a record of the imported interface example:shop/catalog@1.0.0 that holds a resource handle: a derived interface names the consumed types that carry data only",
        ),
        (
            "pub fn bag(bag: Bag) -> u32 { 0 }",
            "pub struct Bag { pub cart: catalog::Cart }",
            "`Bag` holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: a derived function takes such a handle as a parameter, alone or in an option, a result, a list or a tuple, but a record or a variant of the module holds none",
        ),
        (
            "pub fn order() { Ordered::run(Order { item: todo!() }) }",
            "#[hook(composition = series, input = Order)]
            pub struct Ordered;
            pub struct Order { pub item: catalog::Item }",
            "`Order`, which the input of the hook `Ordered` reaches, holds `Item`, a record of the imported interface example:shop/catalog@1.0.0: a derived hook's input holds the types of the module, not those of a consumed interface",
        ),
        (
            "pub fn order() {}",
            "#[hook(composition = series, input = catalog::Size)]
            pub struct Ordered;",
            "the input of the hook `Ordered` holds `Size`, an enum of the imported interface example:shop/catalog@1.0.0: a derived hook's input holds the types of the module, not those of a consumed interface",
        ),
        (
            "pub fn price(item: catalog::Item) -> u32 { 0 } pub fn mine() -> Item { todo!() }",
            "pub struct Item { pub count: u32 }",
            "`Item`, used from example:shop/catalog@1.0.0, and the type `Item` are both `item` in the interface `shopping`",
        ),
    ] {
        let error = refused(shopping(&dir), &service(functions, items));
        assert!(error.contains(message), "{functions}\n{error}");
    }
    let error = refused(
        options(
            "example:clock@1.0.0",
            None,
            &["wasi:clocks/monotonic-clock@0.3.0"],
        ),
        &service(
            "pub fn wait(span: monotonic_clock::Duration) -> u64 { span }",
            "",
        ),
    );
    assert!(
        error.contains("`Shopping::wait` takes `Duration`, a type of the WASI interface wasi:clocks/monotonic-clock@0.3.0: the types of a WASI interface stay out of a derived interface; use a type of the module"),
        "{error}"
    );
}

/// A consumed package whose interface uses the bundled WASI packages: the filesystem's descriptor
/// and the system clock's instant, which it aliases too, next to a record of its own.
const FILES: &str = "package example:files@1.0.0;

interface files {
  use wasi:filesystem/types@0.3.0.{descriptor};
  use wasi:clocks/system-clock@0.3.0.{instant};
  type stamp = instant;
  record item { name: string, touched: u64 }
  count: func() -> u32;
}
";

/// A directory holding one consumed WIT package that uses WASI packages, each bundled package it
/// needs in `deps/` as its file, as a contract `cargo tembra build` wrote holds them.
fn consumed_with_wasi(name: &str, wit: &str, deps: &[&str]) -> PathBuf {
    let dir = consumed(name, wit);
    std::fs::create_dir_all(dir.join("deps")).unwrap();
    for file in deps {
        let (_, source) = tembra_contracts::WASI_SOURCES
            .iter()
            .find(|(known, _)| known == file)
            .expect("a bundled WASI package");
        std::fs::write(dir.join("deps").join(file), source).unwrap();
    }
    dir
}

/// A type or a resource a consumed interface `use`s from a WASI interface is the WASI interface's,
/// through the alias: a derived function taking the filesystem's descriptor through `files` is
/// refused as taking a WASI handle, which the runtime moves between no Orbs, alone or in a
/// container, and a WASI record reached so, or an alias of one, stays out of a derived interface
/// as a directly named WASI type does; the interface's own types stay in.
#[test]
fn a_wasi_type_a_consumed_interface_uses_is_refused_as_a_wasi_type() {
    let dir = consumed_with_wasi("files", FILES, &["wasi-clocks.wit", "wasi-filesystem.wit"]);
    let filing = || {
        options(
            "example:keeper@1.0.0",
            Some(&dir),
            &["example:files/files@1.0.0"],
        )
    };
    let keeper = |functions: &str| {
        format!(
            "mod filing {{
                #[service]
                pub struct Keeper;
                impl Keeper {{ {functions} }}
            }}"
        )
    };
    let module = expanded(
        filing(),
        &keeper("pub fn keep(item: files::Item) -> u32 { item.touched as u32 }"),
    );
    let text = resolved(&module, &dir);
    assert!(
        text.contains("interface keeper {\n  use example:files/files@1.0.0.{item};\n  keep: func(item: item) -> u32;\n}\n"),
        "{text}"
    );
    let handle = "the runtime moves no WASI handle between Orbs";
    let stays_out =
        "the types of a WASI interface stay out of a derived interface; use a type of the module";
    for (functions, message) in [
        (
            "pub fn keep(d: files::Descriptor) -> u32 { 0 }",
            format!(
                "`Keeper::keep` takes `Descriptor`, a resource of the imported interface example:files/files@1.0.0, which it uses from the WASI interface wasi:filesystem/types@0.3.0: {handle}"
            ),
        ),
        (
            "pub fn keep(d: Option<files::Descriptor>) -> u32 { 0 }",
            format!(
                "`Keeper::keep` takes `Descriptor`, a resource of the imported interface example:files/files@1.0.0, which it uses from the WASI interface wasi:filesystem/types@0.3.0: {handle}"
            ),
        ),
        (
            "pub fn when() -> files::Instant { todo!() }",
            format!(
                "`Keeper::when` returns `Instant`, a type of the imported interface example:files/files@1.0.0, which it uses from the WASI interface wasi:clocks/system-clock@0.3.0: {stays_out}"
            ),
        ),
        (
            "pub fn when(stamp: files::Stamp) -> u64 { 0 }",
            format!(
                "`Keeper::when` takes `Stamp`, a type of the imported interface example:files/files@1.0.0, which it uses from the WASI interface wasi:clocks/system-clock@0.3.0: {stays_out}"
            ),
        ),
    ] {
        let error = refused(filing(), &keeper(functions));
        assert!(error.contains(&message), "{functions}\n{error}");
    }
}

/// A state holds a consumed type whose bindings `serde` writes, in its fields or in the module
/// types they reach, whether it stays out of the WIT, with serde's derives the macro adds, or
/// enters it; a resource, a type holding a resource handle and a consumed flags are refused.
#[test]
fn a_state_holds_the_consumed_types_serde_writes() {
    let dir = consumed("state", SHOP);
    let module = |fields: &str, functions: &str| {
        format!(
            "mod shopper {{
                pub struct Shelf {{ pub slots: Vec<catalog::Slot> }}
                pub struct Rack {{ pub tagged: catalog::Tagged }}
                #[state]
                pub struct Kept {{ {fields} }}
                #[service]
                pub struct Shopping;
                impl Shopping {{
                    pub fn keep(item: catalog::Item) -> u32 {{ 0 }}
                    {functions}
                }}
            }}"
        )
    };
    let fields = "pub last: Option<catalog::Item>, pub all: Vec<catalog::Item>, pub size: Option<catalog::Size>";
    let out = expanded(shopping(&dir), &module(fields, ""));
    let generated = tokens(&out);
    for expected in [
        "# [derive (:: serde :: Serialize , :: serde :: Deserialize)] pub struct Kept { pub last : Option < catalog :: Item > , pub all : Vec < catalog :: Item > , pub size : Option < catalog :: Size > }",
        "additional_derives : [:: serde :: Serialize , :: serde :: Deserialize]",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
    assert!(!wit(&out).contains("record kept"), "{}", wit(&out));
    let entered = expanded(
        shopping(&dir),
        &module(fields, "pub fn stored() -> Kept { Kept::read() }"),
    );
    let text = resolved(&entered, &dir);
    assert!(
        text.contains("interface shopping {\n  use example:shop/catalog@1.0.0.{item, size};\n  record kept {\n    last: option<item>,\n    all: list<item>,\n    size: option<size>,\n  }\n"),
        "{text}"
    );
    for (fields, message) in [
        (
            "pub marks: catalog::Marks",
            "the state `Kept` holds `Marks`, a flags of the imported interface example:shop/catalog@1.0.0: the bindings give it no `serde` implementation, and the kernel keeps a state as JSON",
        ),
        (
            "pub rack: Rack",
            "the state `Kept` holds `Rack`, which holds `Tagged`, a record of the imported interface example:shop/catalog@1.0.0 that holds a flags: the bindings give it no `serde` implementation, and the kernel keeps a state as JSON",
        ),
        (
            "pub cart: Option<catalog::Cart>",
            "the state `Kept` holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: a state holds data, which the kernel keeps as JSON",
        ),
        (
            "pub shelf: Shelf",
            "the state `Kept` holds `Shelf`, which holds `Slot`, a record of the imported interface example:shop/catalog@1.0.0 that holds a resource handle: a state holds data, which the kernel keeps as JSON",
        ),
    ] {
        let error = refused(shopping(&dir), &module(fields, ""));
        assert!(error.contains(message), "{fields}\n{error}");
    }
}
