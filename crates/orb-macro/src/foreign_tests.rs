//! The types of the consumed interfaces in derived signatures: the `use` lines the derived WIT
//! gains, the bridges that pass the author's values as they are, the states that hold them, and
//! what is refused where.
use super::Options;
use super::tests::{
    expanded_with as expanded, literal, refused_with as refused, resolved_with, tokens, wit,
};
use std::path::{Path, PathBuf};
use syn::ItemMod;

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

/// A consumed package whose `texts` interface has a resource and nothing else, whose handle the
/// records and variants of a module hold.
const TEXTS: &str = "package example:texts@1.0.0;

/// Texts under construction.
interface texts {
  /// A text that grows as words are appended.
  resource builder {
    constructor(text: string);
    /// Append `word` and return the length of the text.
    append: func(word: string) -> u32;
  }
}
";

/// A directory holding one consumed WIT package, under this module's prefix.
fn consumed(name: &str, wit: &str) -> PathBuf {
    super::tests::consumed(&format!("foreign-{name}"), wit)
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

/// The derived WIT, after checking that it resolves next to the consumed packages of `dir` and the
/// kernel's package a state imports from.
fn resolved(module: &ItemMod, dir: &Path) -> String {
    let text = wit(module);
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str("guest.wit", tembra_contracts::guest::SOURCE)
        .unwrap();
    resolve.push_dir(dir).unwrap();
    resolved_with(resolve, &text);
    text
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

/// A consumed interface's type resolves through a `use` of it as through its module: in the Orb
/// module and in a sub-module, renamed with `as`, in a group and through an alias that is only a
/// path. The derived interface `use`s each by its WIT name, whatever name the module writes.
#[test]
fn a_consumed_type_resolves_through_a_use_of_it() {
    let dir = consumed("used", SHOP);
    let module = expanded(
        shopping(&dir),
        "mod shopper {
            use catalog::Item;
            pub mod till {
                use super::catalog::{Size as Measure, Status};
                pub type Tag = super::catalog::Label;
                #[service]
                pub struct Till;
                impl Till {
                    pub fn price(item: super::Item) -> u32 { item.price }
                    pub fn fit(size: Measure, status: Option<Status>) -> Tag { todo!() }
                }
            }
        }",
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface till {\n  use example:shop/catalog@1.0.0.{item, size, status, label};\n",
        "  price: func(item: item) -> u32;\n",
        "  fit: func(size: size, status: option<status>) -> label;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

/// A record or a variant of the module holds the handle of an imported interface's resource where
/// only parameters reach it, of a service's functions and of a resource's constructor and methods
/// alike, as the runtime moves such a handle anywhere into an argument: the interface defining the
/// type `use`s the resource, and the bridge passes the bindings' type as it is, with the bindings'
/// handle in the resource's place.
#[test]
fn a_parameter_record_holds_an_imported_interfaces_handle() {
    let dir = consumed("job", TEXTS);
    let module = expanded(
        options(
            "example:speller@1.0.0",
            Some(&dir),
            &["example:texts/texts@1.0.0"],
        ),
        "mod spelling {
            /// A word to append to the caller's builder.
            pub struct Job { pub builder: texts::Builder, pub word: String }
            pub enum Task { Spell(Job), Skip(u32) }
            #[wit_resource(service = Speller)]
            pub struct Draft { length: core::cell::Cell<u32> }
            impl Draft {
                #[constructor]
                pub fn new(job: Job) -> Self { Draft { length: core::cell::Cell::new(job.builder.append(&job.word)) } }
                pub fn add(&self, job: Job) -> u32 { job.builder.append(&job.word) }
            }
            #[service]
            pub struct Speller;
            impl Speller {
                pub fn spell_job(job: Job) -> u32 { job.builder.append(&job.word) }
                pub fn run(tasks: Vec<Task>, last: Option<Job>) -> u32 { 0 }
            }
        }",
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface speller {\n  use example:texts/texts@1.0.0.{builder};\n",
        "  record job {\n    builder: builder,\n    word: string,\n  }\n",
        "  variant task {\n    spell(job),\n    skip(u32),\n  }\n",
        "    constructor(job: job);\n",
        "    add: func(job: job) -> u32;\n",
        "  spell-job: func(job: job) -> u32;\n",
        "  run: func(tasks: list<task>, last: option<job>) -> u32;\n",
        "world spelling {\n  import example:texts/texts@1.0.0;\n  export speller;\n}\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let generated = tokens(&module);
    for expected in [
        "fn spell_job (job : Job) -> u32 { Speller :: spell_job (job) }",
        "fn run (tasks : Vec < Task > , last : Option < Job >) -> u32 { Speller :: run (tasks , last) }",
        "fn new (job : Job) -> Self { Draft :: new (job) }",
        "fn add (& self , job : Job) -> u32 { Draft :: add (self , job) }",
        "pub use exports :: example :: speller :: speller :: { Job , Task } ;",
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}

/// A name a result writes resolves to the type of the module writing it before the handle rule
/// looks into it: a `Job` returned in one module is not the `Job` of another that holds an imported
/// handle, so the two get the error for a name that enters the WIT twice, not the handle's.
#[test]
fn a_returned_type_resolves_in_its_module_before_the_handle_rule() {
    let dir = consumed("twice", TEXTS);
    let error = refused(
        options(
            "example:speller@1.0.0",
            Some(&dir),
            &["example:texts/texts@1.0.0"],
        ),
        "mod spelling {
            pub mod jobs {
                pub struct Job { pub builder: super::texts::Builder, pub word: String }
                #[service]
                pub struct Speller;
                impl Speller { pub fn spell_job(job: Job) -> u32 { job.builder.append(&job.word) } }
            }
            pub mod counts {
                pub struct Job { pub count: u32 }
                #[service]
                pub struct Counting;
                impl Counting { pub fn total() -> Job { Job { count: 0 } } }
            }
        }",
    );
    assert!(
        error.contains(
            "`Job` is declared in `jobs` and in `counts`; a name that enters the WIT is one across the Orb module"
        ),
        "{error}"
    );
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

/// A view's module re-exports only the types the interface's functions name; a type of the
/// interface reached only through a field of one of them is no type of the view, which rustc reports
/// on the path, and a type of the interface named like the view's resource is the view's object.
#[test]
fn a_derived_signature_names_only_the_types_a_view_uses() {
    let dir = consumed(
        "view-types",
        "package example:counters@1.0.0;

interface counter {
  record count { value: u32 }
  record counter { at: u32 }
  record tally { key: list<string>, count: count, counter: counter }
  read: async func() -> tally;
}
",
    );
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
        &routing("pub fn total(tally: counter_ref::Tally) -> u32 { 0 }"),
    );
    assert!(
        resolved(&module, &dir).contains("  total: func(tally: tally) -> u32;\n"),
        "{}",
        wit(&module)
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

/// An imported interface whose types are its own but which `use`s the book, the interface the
/// keeper implements.
const REPORTS: &str = "package example:ledger@1.0.0;

interface book {
  record entry { label: string, amount: s64 }
  add: func(entry: entry) -> u32;
}

interface reports {
  use book.{entry};
  record report { entries: list<entry>, total: s64 }
  latest: func() -> report;
}
";

/// A derived interface naming a type of an imported interface `use`s it from the import, and an
/// import of the world that depends on an interface the world exports, here the implemented book,
/// is a world WIT refuses: such a type is refused where it is named, while the module importing
/// the interface without naming its types expands.
#[test]
fn a_type_of_an_import_depending_on_an_export_is_refused() {
    let dir = consumed("reports", REPORTS);
    let summing = || {
        options(
            "example:keeper@1.0.0",
            Some(&dir),
            &["example:ledger/reports@1.0.0"],
        )
    };
    let keeper = |functions: &str| {
        format!(
            "mod keeper {{
                #[service(\"example:ledger/book@1.0.0\")]
                pub struct Keeper;
                impl Keeper {{ pub fn add(entry: book::Entry) -> u32 {{ 0 }} }}
                #[service]
                pub struct Summing;
                impl Summing {{ {functions} }}
            }}"
        )
    };
    let module = expanded(summing(), &keeper("pub fn count() -> u32 { 0 }"));
    let text = resolved(&module, &dir);
    assert!(
        text.contains("  import example:ledger/reports@1.0.0;\n  export example:ledger/book@1.0.0;\n  export summing;\n"),
        "{text}"
    );
    let error = refused(
        summing(),
        &keeper("pub fn sum(r: reports::Report) -> u32 { 0 }"),
    );
    assert!(
        error.contains("`Summing::sum` takes `Report`, a record of the imported interface example:ledger/reports@1.0.0: that interface depends on the implemented interface example:ledger/book@1.0.0, so a derived interface naming `Report` would make the world import an interface depending on one it exports, which WIT refuses; use a type of the module"),
        "{error}"
    );
}

/// A consumed type holding data the runtime refuses in a signature, an option of an option, a map
/// or a list of a fixed length, is refused where a derived signature names it, while the
/// interface's other types are named.
#[test]
fn a_consumed_type_the_runtime_refuses_in_a_signature_is_refused() {
    let dir = consumed(
        "unsupported",
        "package example:shop@1.0.0;

interface catalog {
  record item { name: string, price: u32 }
  record pending { price: option<option<u32>> }
  record index { prices: map<string, u32> }
  type corners = list<u32, 4>;
  lookup: func(name: string) -> option<item>;
}
",
    );
    let shopper = |functions: &str| {
        format!(
            "mod shopper {{
                #[service]
                pub struct Shopping;
                impl Shopping {{ {functions} }}
            }}"
        )
    };
    let module = expanded(
        shopping(&dir),
        &shopper("pub fn price(item: catalog::Item) -> u32 { item.price }"),
    );
    assert!(
        resolved(&module, &dir).contains("  price: func(item: item) -> u32;\n"),
        "{}",
        wit(&module)
    );
    for (functions, message) in [
        (
            "pub fn update(pending: catalog::Pending) -> u32 { 0 }",
            "`Shopping::update` takes `Pending`, a record of the imported interface example:shop/catalog@1.0.0 that holds an option of an option, which the runtime refuses in a signature",
        ),
        (
            "pub fn index() -> Option<catalog::Index> { None }",
            "`Shopping::index` returns `Index`, a record of the imported interface example:shop/catalog@1.0.0 that holds a map, which the runtime refuses in a signature",
        ),
        (
            "pub fn corners(corners: Vec<catalog::Corners>) -> u32 { 0 }",
            "`Shopping::corners` takes `Corners`, a type of the imported interface example:shop/catalog@1.0.0 that holds a list of a fixed length, which the runtime refuses in a signature",
        ),
    ] {
        let error = refused(shopping(&dir), &shopper(functions));
        assert!(error.contains(message), "{functions}\n{error}");
    }
}

/// A consumed type that is an option, `type opt = option<u32>` or an alias of one, is named alone
/// or in a list, while an `Option` around it is an option of an option, which the runtime refuses
/// in a signature: it is refused where it is written, through an import's module or a view's, in
/// a function or in a field of a type that enters the WIT.
#[test]
fn an_option_of_a_consumed_option_is_refused() {
    let dir = consumed(
        "options",
        "package example:shop@1.0.0;

interface catalog {
  type opt = option<u32>;
  type again = opt;
  price: func(name: string) -> opt;
}

interface counter {
  type maybe = option<u32>;
  read: func() -> maybe;
}
",
    );
    let options = || Options {
        keyed: vec![literal("example:shop/counter@1.0.0")],
        ..shopping(&dir)
    };
    let shopper = |items: &str, functions: &str| {
        format!(
            "mod shopper {{
                {items}
                #[service]
                pub struct Shopping;
                impl Shopping {{ {functions} }}
            }}"
        )
    };
    let module = expanded(
        options(),
        &shopper(
            "",
            "pub fn price() -> catalog::Opt { None }
            pub fn count(again: catalog::Again) -> u32 { 0 }
            pub fn read() -> counter_ref::Maybe { None }
            pub fn listed() -> Vec<catalog::Opt> { Vec::new() }",
        ),
    );
    let text = resolved(&module, &dir);
    for expected in [
        "  price: func() -> opt;\n",
        "  count: func(again: again) -> u32;\n",
        "  read: func() -> maybe;\n",
        "  listed: func() -> list<opt>;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let nested = "an option of an option, which the runtime refuses in a signature; use an enum of the module with a case for each";
    for (items, functions, message) in [
        (
            "",
            "pub fn find() -> Option<catalog::Opt> { None }",
            format!(
                "`Shopping::find` returns `Opt`, a type of the imported interface example:shop/catalog@1.0.0, in an `Option`, and `Opt` is an option itself: {nested}"
            ),
        ),
        (
            "",
            "pub fn find(again: Vec<Option<catalog::Again>>) -> u32 { 0 }",
            format!(
                "`Shopping::find` takes `Again`, a type of the imported interface example:shop/catalog@1.0.0, in an `Option`, and `Again` is an option itself: {nested}"
            ),
        ),
        (
            "",
            "pub fn find() -> Option<counter_ref::Maybe> { None }",
            format!(
                "`Shopping::find` returns `Maybe`, a type of the interface example:shop/counter@1.0.0, which this module imports by key, in an `Option`, and `Maybe` is an option itself: {nested}"
            ),
        ),
        (
            "pub struct Found { pub at: Option<catalog::Opt> }",
            "pub fn last() -> Found { todo!() }",
            format!(
                "`Found` holds `Opt`, a type of the imported interface example:shop/catalog@1.0.0, in an `Option`, and `Opt` is an option itself: {nested}"
            ),
        ),
    ] {
        let error = refused(options(), &shopper(items, functions));
        assert!(error.contains(&message), "{functions}\n{error}");
    }
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

/// A hook the module contributes to or declares is exported by the world, as an implemented
/// interface is, so the view of its keyed import names the import's copy of the hook's types,
/// which a derived interface, exported, does not take: a derived signature names a contributed
/// hook's types through the hook's module, the export's copy, rustc reporting the view's on the
/// signature, and none of a declared hook's.
#[test]
fn a_keyed_view_of_an_exported_hook_names_the_imports_copy() {
    let dir = consumed("hooked-views", LEDGER);
    let keyed = |hook: &str| Options {
        keyed: vec![literal(hook)],
        ..options("example:keeper@1.0.0", Some(&dir), &[])
    };
    // A contribution beside the view of its hook.
    let module = expanded(
        keyed("example:ledger/guard@1.0.0"),
        &keeper("pub fn judged(admission: guard::Admission) -> bool { admission.blocked }"),
    );
    let generated = tokens(&module);
    for reexport in [
        "pub use exports :: example :: ledger :: guard ;",
        "pub use self :: example :: keeper :: guard_ref ;",
    ] {
        assert!(generated.contains(reexport), "{reexport}\n{generated}");
    }
    // A declaration beside the view of its hook: the Orb module re-exports the hook's import.
    let gatekeeper = |admit: &str| {
        format!(
            "mod gatekeeper {{
                #[hook(\"example:ledger/gate@1.0.0\", composition = waterfall)]
                pub struct Gate;
                #[service]
                pub struct Admissions;
                impl Admissions {{ {admit} }}
            }}"
        )
    };
    expanded(
        keyed("example:ledger/gate@1.0.0"),
        &gatekeeper("pub fn admit(value: i32) -> i32 { Gate::run(gate::Ticket { value }).value }"),
    );
    let error = refused(
        keyed("example:ledger/gate@1.0.0"),
        &gatekeeper("pub fn admit(ticket: gate_ref::Ticket) -> i32 { ticket.value }"),
    );
    assert!(
        error.contains("`Admissions::admit` takes `Ticket`, a record of the interface example:ledger/gate@1.0.0, which this module imports by key and declares: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `gate_ref::Ticket`, `gate::Ticket` and `Gate::run` are the import's; use a type of the module"),
        "{error}"
    );
}

/// Hooks whose types are `use`d from an interface the world only imports, beside one the hook
/// defines itself.
const SHARED: &str = "package example:ledger@1.0.0;

interface types {
  record admission { value: s32, blocked: bool }
  record ticket { value: s32 }
}

interface guard {
  use types.{admission};
  record verdict { blocked: bool }
  apply: func(input: admission) -> admission;
}

interface gate {
  use types.{ticket};
  record stamp { value: s32 }
  apply: func(input: ticket) -> ticket;
}
";

/// A type an exported hook `use`s from an interface the world only imports has no copy of its own
/// in the hook's export: the bindings alias the hook's import and export, and the view of its
/// keyed import, to the type of that import, so the bindings' module of the hook's import and the
/// view name the very type a derived interface takes, and a derived signature names it through
/// either. A type the hook defines itself has two copies, and rustc reports the import's on the
/// signature.
#[test]
fn a_type_an_exported_hook_uses_from_an_import_has_one_copy() {
    let dir = consumed("shared-types", SHARED);
    let keyed = |hook: &str| Options {
        keyed: vec![literal(hook)],
        ..options("example:keeper@1.0.0", Some(&dir), &[])
    };
    let keeper = |report: &str| {
        format!(
            "mod keeper {{
                #[hook(\"example:ledger/guard@1.0.0\", priority = 1)]
                pub fn admit(input: guard::Admission) -> guard::Admission {{ input }}
                #[service]
                pub struct Report;
                impl Report {{ {report} }}
            }}"
        )
    };
    let module = expanded(
        keyed("example:ledger/guard@1.0.0"),
        &keeper(
            "pub fn judged(admission: self::example::ledger::guard::Admission) -> bool { admission.blocked }
            pub fn viewed(admission: guard_ref::Admission) -> bool { admission.blocked }",
        ),
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface report {\n  use example:ledger/guard@1.0.0.{admission};\n",
        "  judged: func(admission: admission) -> bool;\n",
        "  viewed: func(admission: admission) -> bool;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    // The view of a hook the module declares names the type the hook's import and export alias.
    let module = expanded(
        keyed("example:ledger/gate@1.0.0"),
        "mod gatekeeper {
            #[hook(\"example:ledger/gate@1.0.0\", composition = waterfall)]
            pub struct Gate;
            #[service]
            pub struct Admissions;
            impl Admissions {
                pub fn admit(ticket: gate_ref::Ticket) -> i32 { ticket.value }
                pub fn ran(value: i32) -> gate_ref::Ticket { Gate::run(gate::Ticket { value }) }
            }
        }",
    );
    assert!(
        resolved(&module, &dir)
            .contains("interface admissions {\n  use example:ledger/gate@1.0.0.{ticket};\n"),
        "{}",
        wit(&module)
    );
}

/// The Orb module re-exports the import of a hook it declares, whose types `run` takes and
/// returns, while a derived interface takes the hook's export: a type the hook `use`s from an
/// interface the world only imports is one type under both, which a derived signature names
/// through the hook's module, and a type the hook defines itself has two copies, the module's
/// the import's, which is refused.
#[test]
fn a_type_a_declared_hook_uses_from_an_import_has_one_copy() {
    let dir = consumed("declared-shared", SHARED);
    let gatekeeper = |admit: &str| {
        format!(
            "mod gatekeeper {{
                #[hook(\"example:ledger/gate@1.0.0\", composition = waterfall)]
                pub struct Gate;
                #[service]
                pub struct Admissions;
                impl Admissions {{ {admit} }}
            }}"
        )
    };
    let module = expanded(
        options("example:gatekeeper@1.0.0", Some(&dir), &[]),
        &gatekeeper(
            "pub fn admit(ticket: gate::Ticket) -> i32 { ticket.value }
            pub fn ran(value: i32) -> gate::Ticket { Gate::run(gate::Ticket { value }) }",
        ),
    );
    let text = resolved(&module, &dir);
    for expected in [
        "interface admissions {\n  use example:ledger/gate@1.0.0.{ticket};\n",
        "  admit: func(ticket: ticket) -> s32;\n",
        "  ran: func(value: s32) -> ticket;\n",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let error = refused(
        options("example:gatekeeper@1.0.0", Some(&dir), &[]),
        &gatekeeper("pub fn stamped(stamp: gate::Stamp) -> i32 { stamp.value }"),
    );
    assert!(
        error.contains("`Admissions::stamped` takes `Stamp`, a record of the hook example:ledger/gate@1.0.0, which this module declares: the world exports the hook too, as the portable hook contract requires, and a derived interface takes the names of that export, while `gate::Stamp` and `Gate::run` are the import's; use a type of the module"),
        "{error}"
    );
}

/// The world holds a consumed interface in one role, and the Orb module re-exports one module for
/// it: an interface imported plainly beside a hook the module contributes to or declares,
/// implemented beside such a hook, or imported or implemented twice is an error on the second
/// import literal or item, naming both roles. Each role alone expands.
#[test]
fn one_interface_in_two_roles_is_refused() {
    let dir = consumed("roles", LEDGER);
    let orb = |imports: &[&str], items: &str| {
        let module = format!(
            "mod roles {{
                #[service]
                pub struct Report;
                impl Report {{ pub fn count() -> u32 {{ 0 }} }}
                {items}
            }}"
        );
        (options("example:roles@1.0.0", Some(&dir), imports), module)
    };
    let contribution = "#[hook(\"example:ledger/guard@1.0.0\", priority = 1)]
        pub fn admit(input: guard::Admission) -> guard::Admission { input }";
    let declaration = "#[hook(\"example:ledger/gate@1.0.0\", composition = waterfall)]
        pub struct Gate;";
    let guarding = "#[service(\"example:ledger/guard@1.0.0\")]
        pub struct Guarding;
        impl Guarding { pub fn apply(input: guard::Admission) -> guard::Admission { input } }";
    let gating = |service: &str| {
        format!(
            "#[service(\"example:ledger/gate@1.0.0\")]
            pub struct {service};
            impl {service} {{ pub fn apply(input: gate::Ticket) -> gate::Ticket {{ input }} }}"
        )
    };
    // A role as the attribute's imports and the module's items.
    let import = |identity| (vec![identity], String::new());
    let item = |text: &str| (Vec::new(), text.to_owned());
    let hooked = "the world imports and exports the hook already, and the Orb module re-exports the hook's module";
    let exported = "the world exports the interface once, for the service or for the hook";
    // The first role, the second, and the error on the second.
    for (first, second, message) in [
        (
            import("example:ledger/guard@1.0.0"),
            item(contribution),
            format!(
                "example:ledger/guard@1.0.0 is imported plainly and a hook this module contributes to: {hooked} as `guard`; remove the import"
            ),
        ),
        (
            import("example:ledger/gate@1.0.0"),
            item(declaration),
            format!(
                "example:ledger/gate@1.0.0 is imported plainly and a hook this module declares: {hooked} as `gate`; remove the import"
            ),
        ),
        (
            item(guarding),
            item(contribution),
            format!(
                "example:ledger/guard@1.0.0 is implemented by this module and a hook it contributes to: {exported}; an Orb implements an interface or contributes to it as a hook, not both"
            ),
        ),
        (
            item(&gating("Gating")),
            item(declaration),
            format!(
                "example:ledger/gate@1.0.0 is implemented by this module and a hook it declares: {exported}; an Orb implements an interface or declares it as a hook, not both"
            ),
        ),
        (
            import("example:ledger/book@1.0.0"),
            import("example:ledger/book@1.0.0"),
            "example:ledger/book@1.0.0 is imported twice: the world imports an interface once; remove this import".to_owned(),
        ),
        (
            import("wasi:random/random@0.3.0"),
            import("wasi:random/random@0.3.0"),
            "wasi:random/random@0.3.0 is imported twice: the world imports an interface once; remove this import".to_owned(),
        ),
        (
            item(&gating("Gating")),
            item(&gating("Other")),
            "example:ledger/gate@1.0.0 is implemented twice by this module: the world exports the interface once; implement it with one service".to_owned(),
        ),
    ] {
        let imports = [first.0.clone(), second.0.clone()].concat();
        let (both, module) = orb(&imports, &format!("{}\n{}", first.1, second.1));
        let error = refused(both, &module);
        assert!(error.contains(&message), "{message}\n{error}");
        for (imports, items) in [first, second] {
            let (alone, module) = orb(&imports, &items);
            expanded(alone, &module);
        }
    }
}

/// The `context` option imports the kernel's interface tembra:guest/context, which the Orb module
/// re-exports as `context`: a service implementing that interface, a hook contributing to it and a
/// hook declaring it would each re-export a second module under that name, and each is refused at
/// its item, naming the option. Without the option the service is refused as well, since the
/// derived WIT does not resolve as a contract, which the signature of its interface needs: the
/// kernel's namespace is reserved; a hook on that interface is refused there too, as the interface
/// has more than one function.
#[test]
fn a_service_or_a_hook_of_the_kernels_context_is_refused_beside_the_context_option() {
    let dir = consumed(
        "kernel-context",
        "package example:notes@1.0.0;\ninterface topics {\n  topic: func() -> string;\n}\n",
    );
    // The kernel's package, as a contract's `deps/` holds it, for the interface's WIT.
    std::fs::create_dir_all(dir.join("deps")).unwrap();
    std::fs::write(
        dir.join("deps").join("tembra-guest.wit"),
        tembra_contracts::guest::SOURCE,
    )
    .unwrap();
    let watching = |context| Options {
        context,
        ..options("example:watcher@1.0.0", Some(&dir), &[])
    };
    let service = "mod watcher {
        #[service(\"tembra:guest/context@1.0.0\")]
        pub struct Kernel;
        impl Kernel {
            pub fn object() -> context::Address { todo!() }
            pub fn principal() -> String { String::new() }
            pub fn cancelled() -> bool { false }
            pub fn cancel() {}
            pub fn log(level: context::Level, message: String) {}
        }
    }";
    let hooked = |hook: &str| {
        format!(
            "mod watcher {{
                #[service]
                pub struct Report;
                impl Report {{ pub fn count() -> u32 {{ 0 }} }}
                {hook}
            }}"
        )
    };
    let contribution = hooked(
        "#[hook(\"tembra:guest/context@1.0.0\", priority = 1)]
        pub fn principal(input: String) -> String { input }",
    );
    let declaration = hooked(
        "#[hook(\"tembra:guest/context@1.0.0\", composition = series)]
        pub struct Watch;",
    );
    let kernel = "and that of the kernel's interface tembra:guest/context@1.0.0, which the `context` option imports; keep one of them";
    for (module, role) in [
        (
            service,
            "the implemented interface tembra:guest/context@1.0.0",
        ),
        (
            contribution.as_str(),
            "the hook tembra:guest/context@1.0.0, which this module contributes to",
        ),
        (
            declaration.as_str(),
            "the hook tembra:guest/context@1.0.0, which this module declares",
        ),
    ] {
        let message = format!(
            "the Orb module would re-export two modules as `context`: that of {role}, {kernel}"
        );
        let error = refused(watching(true), module);
        assert!(error.contains(&message), "{message}\n{error}");
    }
    let error = refused(watching(false), service);
    assert!(
        error.contains("the derived WIT does not resolve as a contract: tembra:guest/context@1.0.0: the tembra namespace is reserved for kernel interfaces"),
        "{error}"
    );
    for module in [&contribution, &declaration] {
        let error = refused(watching(false), module);
        assert!(
            error.contains("hook tembra:guest/context@1.0.0 must declare exactly one callback"),
            "{error}"
        );
    }
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
            "pub fn bag(bag: Bag) -> u32 { 0 } pub fn pack() -> Bag { todo!() }",
            "pub struct Bag { pub cart: catalog::Cart }",
            "`Shopping::pack` returns `Bag`, which holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: the runtime returns a resource only from the interface that defines it, so a derived function takes such a handle, alone or in a record or a variant of the module, but returns none",
        ),
        (
            "pub fn next() -> Vec<Choice> { todo!() }",
            "pub enum Choice { Cart(catalog::Cart), Nothing }",
            "`Shopping::next` returns `Choice`, which holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: the runtime returns a resource only from the interface that defines it",
        ),
        (
            "pub fn open() -> Tally { Tally }",
            "pub struct Bag { pub cart: catalog::Cart }
            #[wit_resource(service = Shopping)]
            pub struct Tally;
            impl Tally { pub fn drain(&self) -> Bag { todo!() } }",
            "`Tally::drain` returns `Bag`, which holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: the runtime returns a resource only from the interface that defines it",
        ),
        (
            "pub fn last() -> Option<Receipt> { None }",
            "pub struct Bag { pub cart: catalog::Cart } pub struct Receipt { pub bag: Bag }",
            "`Shopping::last` returns `Receipt`, which holds `Cart`, a resource of the imported interface example:shop/catalog@1.0.0: the runtime returns a resource only from the interface that defines it",
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
/// types they reach, whether it stays out of the WIT, with the author's serde derives, or enters
/// it; a resource, a type holding a resource handle and a consumed flags are refused.
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
        "pub struct Kept { pub last : Option < catalog :: Item > , pub all : Vec < catalog :: Item > , pub size : Option < catalog :: Size > }",
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

/// A catalog whose entry `serde` writes, beside a record holding a resource handle, and an
/// interface of loans the Orb does not import.
const CATALOG: &str = "package example:shop@1.0.0;

interface catalog {
  record entry { name: string, price: u32 }
  resource cart;
  record slot { cart: cart }
  lookup: func(name: string) -> option<entry>;
  fill: func(slot: slot) -> u32;
}

interface lending {
  resource pen;
  record loan { pen: pen }
}
";

/// The bindings leave a type out of a state's serde derives by its bare name, as they compare the
/// names of `additional_derives_ignore`: a consumed record `serde` writes loses them to a record of
/// the module of the same name that holds a resource handle, and a record of the module to such a
/// consumed record. The macro gives each its derives back by its full name, the module's with the
/// author's, so a state holds it. The ignore list names only the types the bindings generate:
/// `loan`, in an interface the Orb does not import, is not on it.
#[test]
fn a_state_holds_a_type_named_like_one_serde_cannot_write() {
    let dir = consumed("namesake", CATALOG);
    let module = expanded(
        shopping(&dir),
        "mod shopper {
            #[state]
            pub struct Kept { pub last: Option<catalog::Entry>, pub slot: Option<Slot> }
            #[wit_resource]
            pub struct Pen { pub ink: u32 }
            impl Pen { pub fn ink(&self) -> u32 { self.ink } }
            pub struct Entry { pub pen: Pen }
            #[derive(serde::Serialize, Hash)]
            pub struct Slot { pub shelf: u32 }
            #[service]
            pub struct Shopping;
            impl Shopping {
                pub fn lend() -> Entry { todo!() }
                pub fn place() -> Slot { todo!() }
            }
        }",
    );
    let generated = tokens(&module);
    for expected in [
        "additional_derives_ignore : [\"entry\" , \"slot\"] ,",
        concat!(
            "additional_type_attributes : { ",
            "\"example:shopper/shopping@1.0.0/slot\" : [# [derive (:: serde :: Serialize , :: serde :: Deserialize , :: core :: hash :: Hash)]] , ",
            "\"example:shop/catalog@1.0.0/entry\" : [# [derive (:: serde :: Serialize , :: serde :: Deserialize)]] } ,",
        ),
    ] {
        assert!(generated.contains(expected), "{expected}\n{generated}");
    }
}
