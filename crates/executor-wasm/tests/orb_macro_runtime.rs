//! Real macro guests link either std or the explicitly selected freestanding runtime.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Mutex, PoisonError},
};
use tembra::{
    Context, Host, Orb, Reference, Service,
    core::Code,
    portable::{Capability, Delivered, Issued, Nominal, Owned, Payload},
};
use tembra_contracts::ModuleContract;
use tembra_executor_wasm::{
    Binding, Contract, Deterministic, Engine, Install, Limits, SystemCapability,
};
use tembra_orb_native_fixture::calculator;
use wit_component::WitPrinter;
use wit_parser::Resolve;

fn component(variable: &str) -> Vec<u8> {
    let path = std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} is required"));
    std::fs::read(path).unwrap_or_else(|error| panic!("reading {variable}: {error}"))
}

/// The contract of a macro guest as `cargo tembra build` packages it: the WIT the macro wrote into
/// the component, whose world says which interfaces the Orb imports itself, with the packages it
/// consumes nested whole. The world read from the component's types would import every interface a
/// view `use`s the types of, the shelf the Orb implements included, which the contract then pairs
/// with its export as a hook; the kernel's package, which the walker's state imports from, and the
/// WASI packages chance imports from are bundled with the contracts.
fn contract(bytes: &[u8]) -> (String, Contract) {
    let mut text = embedded_wit(bytes);
    let mut consumed = Resolve::default();
    consumed
        .push_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../orb-macro/runtime-fixture/consumed"
        ))
        .unwrap();
    for (id, _) in consumed.packages.iter() {
        let mut printer = WitPrinter::default();
        printer.print_package(&consumed, id, false).unwrap();
        text.push('\n');
        text.push_str(&String::from(printer.output));
    }
    let world = text
        .lines()
        .find_map(|line| line.strip_prefix("world "))
        .map(|rest| rest.trim_end_matches('{').trim())
        .expect("the embedded WIT declares a world");
    let contract = Contract::from_source("runtime.wit", &text, Some(world)).unwrap();
    (text, contract)
}

/// The `tembra:wit` custom section of the component's core module: a component is its 8-byte
/// header and sections of an id byte, a LEB128 size and a payload; a core module, section 1, has
/// the same layout, and its custom sections, 0, start with the LEB128 length of their name.
fn embedded_wit(component: &[u8]) -> String {
    fn size(bytes: &[u8], at: &mut usize) -> usize {
        let (mut value, mut shift) = (0, 0);
        loop {
            let byte = bytes[*at];
            *at += 1;
            value |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }
    fn sections(binary: &[u8]) -> Vec<(u8, &[u8])> {
        let mut sections = Vec::new();
        let mut at = 8;
        while at < binary.len() {
            let id = binary[at];
            at += 1;
            let length = size(binary, &mut at);
            sections.push((id, &binary[at..at + length]));
            at += length;
        }
        sections
    }
    for (id, module) in sections(component) {
        if id != 1 {
            continue;
        }
        for (id, payload) in sections(module) {
            if id != 0 {
                continue;
            }
            let mut at = 0;
            let length = size(payload, &mut at);
            if &payload[at..at + length] == b"tembra:wit" {
                return String::from_utf8(payload[at + length..].to_vec()).unwrap();
            }
        }
    }
    panic!("the macro fixture carries no tembra:wit section");
}

/// The WASI interfaces chance imports, which an installation grants and the contract binds to the
/// host's providers of their grants.
const RANDOM: &str = "wasi:random/random@0.3.0";
const CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.0";

/// The grants of the WASI interfaces chance imports, `random` and `clocks`, without which the
/// kernel refuses to install the fixture.
fn granted() -> Install {
    let mut install = Install::default();
    install.grants.system = [SystemCapability::Clocks, SystemCapability::Random].into();
    install
}

/// The binding of a WASI import to the host's provider of its grant.
fn host(import: &str, provider: &str) -> (String, Binding) {
    let provider = provider.to_owned();
    (import.to_owned(), Binding::Host { provider })
}

/// The bindings of the fixture's imports: the kernel serves the walker's state, the host chance's
/// WASI imports, an Orb provider answers the keyed import's view and the plain import of the same
/// interface, another the texts import, another keeper the view of the shelf the Orb implements,
/// and a fourth the ledger the Orb imports and never calls; the shelf itself, imported for the
/// types its view `use`s, binds to nothing.
fn bindings() -> BTreeMap<String, Binding> {
    BTreeMap::from([
        ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
        host(RANDOM, "random"),
        host(CLOCK, "clocks"),
        view(),
        provider(),
        texts(),
        shelf_view(),
        book(),
    ])
}

/// One guest of the fixture: the runtime it links, which names the Orb the engines load, the WIT
/// and the contract read from its component, and the artifact packaged with the roles it carries.
struct Guest {
    name: &'static str,
    wit: String,
    contract: Contract,
    artifact: Vec<u8>,
}

/// Read both guests and check what needs no engine. Both share one WIT, one model and one plan.
/// The contract depends on the bundled packages it imports from, which `cargo tembra build`
/// writes into `contract/deps/`; it binds each WASI import to the host's provider of its grant,
/// as for any compiled component, and requires four Orbs: the provider of the keyed import's view
/// and of the plain import, the provider of the texts import, the keeper of the shelves the Orb
/// reaches by key, and the keeper of the ledger it imports and never calls, whose interface has a
/// resource and so a provider. The WIT holds the author's records and `use`s the consumed packages'
/// records as they define them: `readings` the tally of the imported interface, `library` and
/// `lending` the book and the topic of the shelf the Orb implements, which its world imports only
/// through the shelf view's `use`, `speller` the builder of the texts, in a record of the module.
/// The roles the component carries pair each view with the interface it views, no hook with the
/// shelf, and the artifact keeps the pairs.
fn guests() -> [Guest; 2] {
    let guests = [
        ("standard", "TEMBRA_WASM_ORB_MACRO_STD"),
        ("freestanding", "TEMBRA_WASM_ORB_MACRO_NO_STD"),
    ]
    .map(|(name, variable)| {
        let bytes = component(variable);
        let (wit, contract) = contract(&bytes);
        let dependencies: Vec<&str> = contract
            .standard_dependencies()
            .into_iter()
            .map(|(file, _)| file)
            .collect();
        assert_eq!(
            dependencies,
            ["wasi-clocks.wit", "wasi-random.wit", "tembra-guest.wit"],
            "{name}"
        );
        // The keyed imports' views and the plain imports need their providers named; the WASI
        // imports need their grants; the shelf the Orb implements needs nothing.
        let orb = |name: &str| tembra_contracts::OrbProvider {
            orb: name.into(),
            api_version: 1,
        };
        let requires = BTreeMap::from([
            (COUNTER_VIEW.to_owned(), orb(COUNTERS)),
            (COUNTER.to_owned(), orb(COUNTERS)),
            (TEXTS.to_owned(), orb(BUILDERS)),
            (SHELF_VIEW.to_owned(), orb(SHELVES)),
            (BOOK.to_owned(), orb(LEDGER)),
        ]);
        assert_eq!(
            contract.providers(&requires, &granted().grants).unwrap(),
            bindings(),
            "{name}"
        );
        let plan = contract.plan("runtime", 1, &bindings()).unwrap();
        let mut imports: Vec<(&str, &str)> = plan
            .host_imports
            .iter()
            .map(|import| (import.contract.as_str(), import.provider.as_str()))
            .collect();
        imports.sort();
        assert_eq!(imports, [(CLOCK, "clocks"), (RANDOM, "random")], "{name}");
        // No WASI import requires an Orb: the provider of the view and of the plain import, the
        // provider of the texts import, the keeper of the shelf view and the keeper of the ledger
        // are the only ones.
        let mut required: Vec<&str> = plan
            .orb
            .requires
            .iter()
            .map(|requirement| requirement.orb.as_str())
            .collect();
        required.sort_unstable();
        assert_eq!(required, [BUILDERS, COUNTERS, LEDGER, SHELVES], "{name}");
        let words: String = wit.split_whitespace().collect();
        for expected in [
            "recordaccess{read:bool,write:bool,share:bool,}",
            "interfacereadings{useexample:counters/counter@1.0.0.{tally};",
            "interfacelibrary{useexample:shelves/shelf@1.0.0.{book,topic};",
            "interfacespeller{useexample:texts/texts@1.0.0.{builder};",
            "recordjob{builder:builder,word:string,}",
            "interfaceshelf-ref{useexample:shelves/shelf@1.0.0.{book};",
            "interfacelending{useexample:shelves/shelf@1.0.0.{book};",
            "importshelf-ref;",
            "exportexample:shelves/shelf@1.0.0;",
            // A case with named fields enters as a record of its own, next to the variant.
            "recordoutcome-refused{reason:string,severity:u32,}",
            "variantoutcome{accepted,refused(outcome-refused),deferred(u32),}",
            "recorddecision{reviewer:string,outcome:outcome,}",
            // Free JSON is a string holding its text, alone, in a list, in an option and in a
            // record of the module.
            "recordnote{title:string,body:string,}",
            "annotated:func(document:string,note:string)->string;",
            "notes:func(documents:list<string>)->list<note>;",
            "object:func(note:note)->option<string>;",
            // A newtype is the WIT type of its field, alone, in a list, in a tuple and in a record.
            "recordmeasure{amount:f64,unit:string,}",
            "measured:func(amount:f64,unit:string)->measure;",
            "tally:func(measures:list<measure>,unit:string)->tuple<u32,list<string>>;",
        ] {
            assert!(words.contains(expected), "{name}: {expected}\n{wit}");
        }
        assert!(
            !words.contains("importexample:shelves/shelf@1.0.0;"),
            "{name}: the world imports the shelf only through the view's `use`\n{wit}"
        );
        let roles = contract.embedded_roles(&bytes).unwrap();
        assert_eq!(
            roles.references,
            BTreeMap::from([
                (COUNTER_VIEW.to_owned(), COUNTER.to_owned()),
                (SHELF_VIEW.to_owned(), SHELF.to_owned()),
            ]),
            "{name}"
        );
        assert!(
            roles.contributions.is_empty() && roles.hooks.is_empty(),
            "{name}: {roles:?}"
        );
        let artifact = contract
            .component_artifact(&bytes, name, 1, &roles)
            .unwrap();
        let decoded = tembra_contracts::Artifact::decode(&artifact).unwrap();
        for (view, interface) in [(COUNTER_VIEW, COUNTER), (SHELF_VIEW, SHELF)] {
            assert_eq!(
                decoded.contract.references()[view].interface,
                interface,
                "{name}"
            );
        }
        Guest {
            name,
            wit,
            contract,
            artifact,
        }
    });
    let [standard, freestanding] = &guests;
    assert_eq!(standard.wit, freestanding.wit);
    assert_eq!(standard.contract.model(), freestanding.contract.model());
    // Planned under one name, so that the plans compare.
    let planned = |guest: &Guest| guest.contract.plan("runtime", 1, &bindings()).unwrap().orb;
    assert_eq!(planned(standard), planned(freestanding));
    guests
}

#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn std_and_freestanding_guests_execute_compiled() {
    execute(Engine::new(Limits::default()).unwrap());
}

#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn std_and_freestanding_guests_execute_interpreted() {
    execute(Engine::interpreted(Limits::default()).unwrap());
}

/// Load both guests into `engine` once, beside the native providers of the interfaces they import,
/// and drive every service of each in one host; the engine's secure random source is fixed for
/// chance's draws. The installations the kernel refuses are checked after the good loads. Both
/// guests reach the provider's `main` object, the host's, in `read`: the second counts on.
fn execute(engine: Engine) {
    let engine = engine.secure_random(|| Deterministic::new(vec![42]));
    let guests = guests();
    let loaded: Vec<Orb> = guests
        .iter()
        .map(|guest| engine.load(&guest.artifact, bound(granted())).unwrap())
        .collect();
    refuse_ungranted(&engine, &guests);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let orbs = loaded
        .into_iter()
        .chain([
            counters(),
            builders(),
            shelves(),
            ledger(&guests[0].contract),
        ])
        .collect();
    host.install(orbs).unwrap();
    for (guest, counted) in guests.iter().zip([0, 7]) {
        let name = guest.name;
        greet(&mut pool, &host, name);
        walk(&mut pool, &host, name);
        chance(&mut pool, &host, name);
        count(&mut pool, &host, name);
        tally(&mut pool, &host, &engine, name);
        spell(&mut pool, &host, name);
        permit(&mut pool, &host, name);
        read(&mut pool, &host, name, counted);
        shelve(&mut pool, &host, name);
        lend(&mut pool, &host, name);
        review(&mut pool, &host, name);
        document(&mut pool, &host, name);
        measure(&mut pool, &host, name);
    }
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (diagnostics.calls, diagnostics.references, diagnostics.roots),
        (0, 0, 0)
    );
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The kernel refuses an installation of either guest that does not grant both WASI imports,
/// naming the missing one, and a refused Orb runs nothing: the engine keeps the instances of the
/// guests loaded before, and no other. Nor does it compile anything: a refused load finds the
/// component the loaded guest keeps in the engine's cache, where a new compilation would have
/// replaced it with a copy that the refusal releases.
fn refuse_ungranted(engine: &Engine, guests: &[Guest; 2]) {
    let running = engine.instances();
    for guest in guests {
        for (system, missing) in [
            (vec![], ""),
            (vec![SystemCapability::Clocks], RANDOM),
            (vec![SystemCapability::Random], CLOCK),
            (vec![SystemCapability::Cli], ""),
        ] {
            let mut install = bound(Install::default());
            install.grants.system = system.iter().copied().collect();
            let Err(error) = engine.load(&guest.artifact, install) else {
                panic!("{}: installed with only {system:?}", guest.name);
            };
            assert_eq!(
                (error.code, error.reason.as_str()),
                (Code::Forbidden, "orb_grant"),
                "{}: {error}",
                guest.name
            );
            assert!(error.message.contains(missing), "{}: {error}", guest.name);
        }
    }
    assert_eq!(engine.instances(), running, "a refused Orb runs nothing");
    assert_eq!(
        engine.components(),
        guests.len(),
        "a refused Orb compiles nothing"
    );
}

/// Drive the greeting of the Orb `name`: a string crosses both ways through the runtime the guest
/// links.
fn greet(pool: &mut LocalPool, host: &Host, name: &str) {
    let greeting = Rc::new(
        host.get(
            &format!("{name}::example:runtime/greeting@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(greeting.ready()).unwrap();
    for who in ["Ada", "a longer name with spaces", "🦀"] {
        assert_eq!(
            counting_call(pool, host, &greeting, "alice", "hello", json!([who])),
            json!(format!("Hello, {who}!")),
            "{name}"
        );
    }
    greeting.release().unwrap();
}

/// Drive the walker of the Orb `name`, whose types take the derives their author wrote: its state
/// starts from the state's derived `Default`, built from the derived defaults of a record and an
/// enum, and its methods compare, hash or order those types and print them with `Debug`.
fn walk(pool: &mut LocalPool, host: &Host, name: &str) {
    let walker = Rc::new(
        host.get(
            &format!("{name}::example:runtime/walker@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(walker.ready()).unwrap();
    let state = walker.state::<Value>().unwrap();
    assert_eq!(
        *state.value().unwrap(),
        json!({"at": {"x": 0, "y": 0}, "facing": "North", "moves": 0}),
        "{name}"
    );
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &walker, "alice", method, arguments)
    };
    let moves = json!([
        {"tag": "turn", "val": "east"},
        {"tag": "step", "val": 3},
        {"tag": "jump", "val": {"x": -1, "y": 2}},
        {"tag": "rest"},
    ]);
    assert_eq!(
        call(pool, "walk", json!([moves])),
        json!({"at": {"x": -1, "y": 2}, "facing": "east", "moves": 4}),
        "{name}"
    );
    assert_eq!(call(pool, "at-start", json!([])), json!(false), "{name}");
    let repeated = json!([
        {"tag": "step", "val": 1},
        {"tag": "step", "val": 1},
        {"tag": "turn", "val": "north"},
        {"tag": "jump", "val": {"x": 0, "y": 0}},
        {"tag": "jump", "val": {"x": 0, "y": 0}},
        {"tag": "rest"},
        {"tag": "rest"},
    ]);
    assert_eq!(
        call(pool, "distinct", json!([repeated])),
        json!(4),
        "{name}"
    );
    let home = json!([{"tag": "jump", "val": {"x": 0, "y": 0}}, {"tag": "turn", "val": "north"}]);
    call(pool, "walk", json!([home]));
    assert_eq!(call(pool, "at-start", json!([])), json!(true), "{name}");
    assert_eq!(
        call(pool, "describe", json!([])),
        json!("Position { at: Point { x: 0, y: 0 }, facing: North, moves: 6 }"),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"at": {"x": 0, "y": 0}, "facing": "North", "moves": 6}),
        "{name}"
    );
    walker.release().unwrap();
}

/// A native Orb of the macro keeps a `#[state]` as the JSON a component of the macro keeps for the
/// same value: the text serde writes, as the kernel reads it. The native fixture's walker keeps a
/// `Position` with the fields and case names of the runtime fixture's, so the two states start
/// alike, and two walks that end in the same place, each with its fixture's moves, leave the same
/// JSON, which a host reads from either object the same way.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_native_state_keeps_the_json_of_a_component() {
    let bytes = component("TEMBRA_WASM_ORB_MACRO_STD");
    let (_, contract) = contract(&bytes);
    let roles = contract.embedded_roles(&bytes).unwrap();
    let artifact = contract
        .component_artifact(&bytes, "standard", 1, &roles)
        .unwrap();
    let engine = Engine::new(Limits::default()).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine.load(&artifact, bound(granted())).unwrap(),
        counters(),
        builders(),
        shelves(),
        ledger(&contract),
        calculator::orb().unwrap(),
    ])
    .unwrap();
    let mut walker = |service: &str| {
        let walker = Rc::new(host.get(service, vec!["main".into()], "alice").unwrap());
        pool.run_until(walker.ready()).unwrap();
        walker
    };
    let compiled = walker("standard::example:runtime/walker@1.0.0");
    let native = walker("calculator::example:calculator/walker@1.0.0");
    let (compiled_state, native_state) = (
        compiled.state::<Value>().unwrap(),
        native.state::<Value>().unwrap(),
    );
    assert_eq!(
        *native_state.value().unwrap(),
        *compiled_state.value().unwrap()
    );
    counting_call(
        &mut pool,
        &host,
        &compiled,
        "alice",
        "walk",
        json!([[
            {"tag": "turn", "val": "east"},
            {"tag": "step", "val": 3},
            {"tag": "jump", "val": {"x": -1, "y": 2}},
            {"tag": "rest"},
        ]]),
    );
    counting_call(
        &mut pool,
        &host,
        &native,
        "alice",
        "walk",
        json!([[
            {"tag": "step", "val": 2},
            {"tag": "turn", "val": "west"},
            {"tag": "step", "val": 1},
            {"tag": "turn", "val": "east"},
        ]]),
    );
    let walked = json!({"at": {"x": -1, "y": 2}, "facing": "East", "moves": 4});
    assert_eq!(*compiled_state.value().unwrap(), walked);
    assert_eq!(*native_state.value().unwrap(), walked);
    assert_eq!(
        compiled.state_snapshot().unwrap().1,
        native.state_snapshot().unwrap().1
    );
    compiled.release().unwrap();
    native.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Drive the chance of the Orb `name`, which calls WASI through the interfaces its attribute
/// imports: it draws from the engine's secure random source, fixed at 42, and waits on the host's
/// monotonic clock.
fn chance(pool: &mut LocalPool, host: &Host, name: &str) {
    let chance = Rc::new(
        host.get(
            &format!("{name}::example:runtime/chance@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(chance.ready()).unwrap();
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &chance, "alice", method, arguments)
    };
    assert_eq!(
        call(pool, "draw", json!([5])),
        json!([42, 42, 42, 42, 42]),
        "{name}"
    );
    assert_eq!(call(pool, "draw", json!([0])), json!([]), "{name}");
    // The call suspends on the host's clock and resumes when the wait is over.
    let waited = call(pool, "pause", json!([2_000_000])).as_u64().unwrap();
    assert!(waited >= 2_000_000, "{name}: waited {waited} ns");
    chance.release().unwrap();
}

/// The interface the fixture imports by key and plainly, the view the macro derives for the keyed
/// import in the fixture's package, which the installer binds, and the native Orb providing the
/// interface.
const COUNTER: &str = "example:counters/counter@1.0.0";
const COUNTER_VIEW: &str = "example:runtime/counter-ref@1.0.0";
const COUNTERS: &str = "counters";

/// The binding of the keyed import's view to its provider.
fn view() -> (String, Binding) {
    let provider = Binding::Orb {
        name: COUNTERS.into(),
        api_version: 1,
    };
    (COUNTER_VIEW.to_owned(), provider)
}

/// The binding of the plain import to its provider.
fn provider() -> (String, Binding) {
    let provider = Binding::Orb {
        name: COUNTERS.into(),
        api_version: 1,
    };
    (COUNTER.to_owned(), provider)
}

/// The interface the fixture imports for the builders its spelling jobs hold, and the native Orb
/// providing it.
const TEXTS: &str = "example:texts/texts@1.0.0";
const BUILDERS: &str = "builders";

/// The binding of the texts import to its provider.
fn texts() -> (String, Binding) {
    let provider = Binding::Orb {
        name: BUILDERS.into(),
        api_version: 1,
    };
    (TEXTS.to_owned(), provider)
}

/// An installation of the fixture that binds the keyed import's view and the plain import to the
/// provider, the texts import to its own, the view of the shelf the fixture implements to another
/// keeper and the ledger to its keeper; without them the kernel refuses the fixture, as for any
/// import of another Orb.
fn bound(install: Install) -> Install {
    install
        .requires(COUNTER_VIEW, COUNTERS, 1)
        .requires(COUNTER, COUNTERS, 1)
        .requires(TEXTS, BUILDERS, 1)
        .requires(SHELF_VIEW, SHELVES, 1)
        .requires(BOOK, LEDGER, 1)
}

/// The interface the fixture implements and imports by key, the view the macro derives for that
/// import, which the installer binds, and the native Orb keeping the shelves it reaches.
const SHELF: &str = "example:shelves/shelf@1.0.0";
const SHELF_VIEW: &str = "example:runtime/shelf-ref@1.0.0";
const SHELVES: &str = "shelves";

/// The binding of the shelf view to the keeper it reaches.
fn shelf_view() -> (String, Binding) {
    let provider = Binding::Orb {
        name: SHELVES.into(),
        api_version: 1,
    };
    (SHELF_VIEW.to_owned(), provider)
}

/// The native keeper of the shelves the fixture reaches by key, read from the package the fixture
/// consumes and implements: each shelf counts the books put on it, and the key `refused` is
/// refused.
fn shelves() -> Orb {
    let wit = format!(
        "{}\nworld keeper {{ export shelf; }}\n",
        include_str!("../../orb-macro/runtime-fixture/consumed/deps/shelves.wit")
    );
    let service = Contract::from_source("shelves.wit", &wit, Some("keeper"))
        .unwrap()
        .plan(SHELVES, 1, &BTreeMap::new())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new(SHELVES, 1).service(
        Service::new(service.name, |declaration| {
            if declaration.address()?.key == ["refused"] {
                return Err(tembra::Error::new(Code::Forbidden, "key", "unapproved key"));
            }
            Ok(Cell::new(0_u32))
        })
        .signature(service.signature)
        .method("shelve", |books, _, (_book,): (Value,)| async move {
            books.set(books.get() + 1);
            Ok(books.get())
        }),
    )
}

/// The interface the fixture imports plainly and never calls, whose `tally` shares the name of the
/// counters' record, and the native Orb providing it.
const BOOK: &str = "example:ledger/book@1.0.0";
const LEDGER: &str = "ledger";

/// The binding of the ledger import to its provider.
fn book() -> (String, Binding) {
    let provider = Binding::Orb {
        name: LEDGER.into(),
        api_version: 1,
    };
    (BOOK.to_owned(), provider)
}

/// The native keeper of the ledger: the interface has a resource and no function, so the contract
/// requires a provider of it, while `plan` lists no service for an interface without functions. The
/// keeper serves it under the name and with the signature the fixture's own plan requires of its
/// provider, and answers no method, as the fixture calls none.
fn ledger(contract: &Contract) -> Orb {
    let plan = contract.plan("runtime", 1, &bindings()).unwrap();
    let requirement = plan
        .orb
        .requires
        .iter()
        .find(|requirement| requirement.orb == LEDGER)
        .expect("the fixture requires the ledger");
    let mut keeper = Orb::new(LEDGER, 1);
    for (name, signature) in &requirement.signatures {
        keeper = keeper
            .service(Service::new(name.clone(), |_| Ok(())).signature(Some(signature.clone())));
    }
    keeper
}

/// One object of the provider: the key it was acquired with, the principal that acquired it and
/// what it has counted.
struct Counter {
    key: Vec<String>,
    principal: String,
    count: Cell<u32>,
}

/// The native provider of the interface the fixture imports by key, read from the package the
/// fixture consumes: each object counts on its own, and the key `refused` is refused.
fn counters() -> Orb {
    let wit = format!(
        "{}\nworld provider {{ export counter; }}\n",
        include_str!("../../orb-macro/runtime-fixture/consumed/counters.wit")
    );
    let service = Contract::from_source("counters.wit", &wit, Some("provider"))
        .unwrap()
        .plan(COUNTERS, 1, &BTreeMap::new())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new(COUNTERS, 1).service(
        Service::new(service.name, |declaration| {
            let address = declaration.address()?;
            if address.key == ["refused"] {
                return Err(tembra::Error::new(Code::Forbidden, "key", "unapproved key"));
            }
            Ok(Counter {
                key: address.key.clone(),
                principal: address.principal.clone(),
                count: Cell::new(0),
            })
        })
        .signature(service.signature)
        .method("add", |counter, _, (amount,): (u32,)| async move {
            counter.count.set(counter.count.get() + amount);
            Ok(counter.count.get())
        })
        .method("read", |counter, _, _: [Value; 0]| async move {
            Ok(json!({
                "key": counter.key,
                "principal": counter.principal,
                "count": counter.count.get(),
            }))
        }),
    )
}

/// Call `method` of a guest's service `reference` with `arguments`, in a root of `principal`, and
/// return the result, both as the JSON of the portable boundary.
fn counting_call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> Value {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move {
            reference
                .call_portable::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
    .unwrap()
}

/// Drive the `counting` service of the Orb `name`, which reaches the objects of the native
/// provider through the view the macro derived for the fixture's keyed import, packaged with the
/// role the component carries. In one call, two references of one key reach one object and
/// another key's object counts on its own; each principal's calls reach objects of its own; a key
/// the provider refuses comes back as the error of the WIT result, and the revision serves the
/// next call. The provider's record, which the view's module names, crosses `counting` as it is.
fn count(pool: &mut LocalPool, host: &Host, name: &str) {
    let counting = Rc::new(
        host.get(
            &format!("{name}::example:runtime/counting@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(counting.ready()).unwrap();
    for principal in ["alice", "bob"] {
        assert_eq!(
            counting_call(
                pool,
                host,
                &counting,
                principal,
                "count",
                json!(["a", "b", 2])
            ),
            json!({
                "tag": "ok",
                "val": format!("[2, 4, 2] a@{principal}=4 b@{principal}=2"),
            }),
            "{name}"
        );
    }
    let refused = counting_call(
        pool,
        host,
        &counting,
        "alice",
        "count",
        json!(["refused", "b", 1]),
    );
    assert_eq!(refused["tag"], "err", "{name}: {refused}");
    assert!(
        refused["val"]
            .as_str()
            .is_some_and(|reason| reason.contains("unapproved key")),
        "{name}: {refused}"
    );
    assert_eq!(
        counting_call(
            pool,
            host,
            &counting,
            "alice",
            "count",
            json!(["c", "c", 1])
        ),
        json!({"tag": "ok", "val": "[1, 2, 3] c@alice=3 c@alice=3"}),
        "{name}"
    );
    // An object lives while a call references it: the next call's is new.
    assert_eq!(
        counting_call(pool, host, &counting, "bob", "read", json!(["d"])),
        json!({"tag": "ok", "val": {"key": ["d"], "principal": "bob", "count": 0}}),
        "{name}"
    );
    counting.release().unwrap();
}

/// Drive the tallies of the Orb `name`: each owned tally passed to `close`, to `close-all` in a
/// list and an option, to another tally's `absorb`, or to `settle` in the record of a case with
/// named fields, which `open` returns it in, leaves the caller and the provider's handle table,
/// and the guest drops the value it took back.
fn tally(pool: &mut LocalPool, host: &Host, engine: &Engine, name: &str) {
    /// Run one operation under a fresh root of `alice`.
    fn root<T: 'static>(
        pool: &mut LocalPool,
        host: &Host,
        operation: impl AsyncFnOnce(Context) -> tembra::Result<T> + 'static,
    ) -> T {
        pool.run_until(host.root("alice", move |context| operation(context).boxed_local()))
            .unwrap()
    }
    let tallies = Rc::new(
        host.get(
            &format!("{name}::example:runtime/tallies@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(tallies.ready()).unwrap();
    let issue = |pool: &mut LocalPool, method: &'static str, at: u32| -> Owned {
        let tallies = tallies.clone();
        root(pool, host, async move |context| {
            tallies.call_portable(&context, method, (at,)).await
        })
    };
    let call = |pool: &mut LocalPool, method: &'static str, payload: Payload| -> Value {
        let tallies = tallies.clone();
        root(pool, host, async move |context| {
            tallies.call_portable(&context, method, payload).await
        })
    };
    let dropped = |pool: &mut LocalPool| call(pool, "dropped", Payload::data(json!([])));

    let first = issue(pool, "start", 5);
    let (first, total) = root(pool, host, async move |context| {
        let total: u32 = first
            .call_portable(&context, "[method]tally.add", (3_u32,))
            .await?;
        Ok((first, total))
    });
    assert_eq!(total, 8, "{name}");
    assert_eq!(engine.resources(), 1, "{name}");
    let mut payload = Payload::data(json!([0]));
    payload.attach_argument(first).unwrap();
    assert_eq!(call(pool, "close", payload), json!(8), "{name}");
    assert_eq!(engine.resources(), 0, "{name}");
    assert_eq!(dropped(pool), json!(1), "{name}");

    let mut payload = Payload::data(json!([[0, 1], 2]));
    for at in [1, 2, 4] {
        payload.attach_argument(issue(pool, "start", at)).unwrap();
    }
    assert_eq!(engine.resources(), 3, "{name}");
    assert_eq!(call(pool, "close-all", payload), json!([1, 2, 4]), "{name}");
    assert_eq!(engine.resources(), 0, "{name}");
    assert_eq!(dropped(pool), json!(4), "{name}");

    // A tally's method takes another tally; dropping the receiver's handle destroys it too.
    let kept = issue(pool, "[constructor]tally", 10);
    let mut payload = Payload::data(json!([0]));
    payload.attach_argument(issue(pool, "start", 20)).unwrap();
    let total: Value = root(pool, host, async move |context| {
        kept.call_portable(&context, "[method]tally.absorb", payload)
            .await
    });
    assert_eq!(total, json!(30), "{name}");
    assert_eq!(dropped(pool), json!(6), "{name}");
    assert_eq!(engine.resources(), 0, "{name}");

    // A case with named fields holds a tally: the record of the case carries its handle, out and
    // back in, and an opening without one carries none.
    let opened = {
        let tallies = tallies.clone();
        root(pool, host, async move |context| {
            tallies
                .call_portable::<_, Delivered>(&context, "open", (7_u32,))
                .await
        })
    };
    assert_eq!(
        opened.value,
        json!({"tag": "opened", "val": {"tally": 0, "at": 7}}),
        "{name}"
    );
    assert_eq!(engine.resources(), 1, "{name}");
    let mut payload = Payload::data(json!([opened.value]));
    for owned in opened.owned {
        payload.attach_argument(owned).unwrap();
    }
    assert_eq!(call(pool, "settle", payload), json!(7), "{name}");
    assert_eq!(engine.resources(), 0, "{name}");
    assert_eq!(dropped(pool), json!(7), "{name}");

    // A tally named through a `use` that renames it crosses as the tally: wrapped into its handle
    // when returned, and taken back from it in a tuple beside a step.
    let restarted = issue(pool, "restart", 4);
    assert_eq!(engine.resources(), 1, "{name}");
    let mut payload = Payload::data(json!([[0, 3]]));
    payload.attach_argument(restarted).unwrap();
    assert_eq!(call(pool, "close-with", payload), json!(7), "{name}");
    assert_eq!(engine.resources(), 0, "{name}");
    assert_eq!(dropped(pool), json!(8), "{name}");
    let refused = call(pool, "open", Payload::data(json!([2000])));
    assert_eq!(
        refused,
        json!({"tag": "refused", "val": {"reason": "above a thousand"}}),
        "{name}"
    );
    assert_eq!(engine.resources(), 0, "{name}");
    tallies.release().unwrap();
}

/// Drive the permits of the Orb `name`. A record of `bool` fields crosses the boundary both ways:
/// as a parameter and a result, inside a record, a list and an option, and in a state the kernel
/// keeps as JSON. It is an object of every field on the portable boundary, as serde writes it in
/// the state.
fn permit(pool: &mut LocalPool, host: &Host, name: &str) {
    let permits = Rc::new(
        host.get(
            &format!("{name}::example:runtime/permits@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(permits.ready()).unwrap();
    let state = permits.state::<Value>().unwrap();
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &permits, "alice", method, arguments)
    };
    let access = |read: bool, write: bool, share: bool| json!({"read": read, "write": write, "share": share});
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": access(false, false, false), "grants": 0}),
        "{name}"
    );
    assert_eq!(
        call(pool, "invert", json!([access(true, false, true)])),
        access(false, true, false),
        "{name}"
    );
    assert_eq!(
        call(pool, "give", json!(["ada", access(false, true, true)])),
        json!({"who": "ada", "access": access(false, true, true)}),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": access(false, true, true), "grants": 1}),
        "{name}"
    );
    assert_eq!(
        call(pool, "kept", json!([])),
        json!({"last": access(false, true, true), "grants": 1}),
        "{name}"
    );
    let grants = json!([
        {"who": "ada", "access": access(true, false, false)},
        {"who": "bob", "access": access(false, false, true)},
    ]);
    assert_eq!(
        call(pool, "merged", json!([grants, null])),
        access(true, false, true),
        "{name}"
    );
    assert_eq!(
        call(pool, "merged", json!([[], access(false, true, false)])),
        access(false, true, false),
        "{name}"
    );
    permits.release().unwrap();
}

/// Drive the readings of the Orb `name`, whose plain import reaches the provider's `main` object
/// after it has counted `counted`. The tally the plain import reads comes back alone and in a
/// record of the module, goes back in as a parameter, alone and in a list, and the kernel keeps
/// the last one in the readings object's state: the provider's record on the portable boundary
/// and, as the JSON `serde` writes for it, in the state.
fn read(pool: &mut LocalPool, host: &Host, name: &str, counted: u32) {
    let readings = Rc::new(
        host.get(
            &format!("{name}::example:runtime/readings@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(readings.ready()).unwrap();
    let state = readings.state::<Value>().unwrap();
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &readings, "alice", method, arguments)
    };
    let tally = |count: u32| json!({"key": ["main"], "principal": "host", "count": count});
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": null, "readings": 0}),
        "{name}"
    );
    let first = call(pool, "read", json!([3, "first"]));
    let after_first = tally(counted + 3);
    assert_eq!(
        first,
        json!({"tally": after_first, "note": "first"}),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": after_first, "readings": 1}),
        "{name}"
    );
    let second = call(pool, "read", json!([4, "second"]));
    let after_second = tally(counted + 7);
    assert_eq!(
        second,
        json!({"tally": after_second, "note": "second"}),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": after_second, "readings": 2}),
        "{name}"
    );
    assert_eq!(
        call(pool, "total", json!([[first, second], [tally(5)]])),
        json!(2 * counted + 15),
        "{name}"
    );
    assert_eq!(
        call(
            pool,
            "joined",
            json!([{"key": ["a", "b"], "principal": "carol", "count": 2}])
        ),
        json!({"key": ["a/b"], "principal": "carol", "count": 2}),
        "{name}"
    );
    readings.release().unwrap();
}

/// Drive the shelf and the library of the Orb `name`: the shelf takes a book of the interface it
/// implements, and the library takes and returns the same books and topics, alone, in a list, in
/// an option and in a record of the module.
fn shelve(pool: &mut LocalPool, host: &Host, name: &str) {
    let reference = |pool: &mut LocalPool, service: &str| {
        let reference = Rc::new(host.get(service, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        reference
    };
    let call =
        |pool: &mut LocalPool, reference: &Rc<Reference>, method: &'static str, arguments| {
            counting_call(pool, host, reference, "alice", method, arguments)
        };
    let book = |title: &str, pages: u32, topic: &str| json!({"title": title, "pages": pages, "topic": topic});
    let shelf = reference(pool, &format!("{name}::example:shelves/shelf@1.0.0"));
    assert_eq!(
        call(
            pool,
            &shelf,
            "shelve",
            json!([book("dune", 412, "fiction")])
        ),
        json!(1),
        "{name}"
    );
    shelf.release().unwrap();
    let library = reference(pool, &format!("{name}::example:runtime/library@1.0.0"));
    let books = json!([
        book("dune", 412, "fiction"),
        book("cosmos", 365, "science"),
        book("emma", 474, "fiction"),
    ]);
    assert_eq!(
        call(pool, &library, "on-topic", json!([books, "fiction"])),
        json!({
            "books": [book("emma", 474, "fiction"), book("dune", 412, "fiction")],
            "pages": 886,
        }),
        "{name}"
    );
    assert_eq!(
        call(pool, &library, "longest", json!([books])),
        book("emma", 474, "fiction"),
        "{name}"
    );
    assert_eq!(
        call(pool, &library, "longest", json!([[]])),
        Value::Null,
        "{name}"
    );
    library.release().unwrap();
}

thread_local! {
    /// How many builders of the native provider were closed on this thread, each test's own: a
    /// builder is closed when its last holder drops it, the guest that consumed it included.
    static CLOSED: Cell<u32> = const { Cell::new(0) };
}

/// One builder of the native provider of the texts: the text it holds. Its `Drop`, the close of
/// the capability, counts it among the builders closed.
struct Builder {
    nominal: Nominal,
    text: RefCell<String>,
}

impl Capability for Builder {
    fn nominal(&self) -> &Nominal {
        &self.nominal
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        CLOSED.set(CLOSED.get() + 1);
    }
}

/// The native provider of the interface the fixture's spelling jobs hold a builder of, read from
/// the package the fixture consumes: its constructor issues a builder as a capability, which
/// `append` finds again as the call's `self`.
fn builders() -> Orb {
    let wit = format!(
        "{}\nworld provider {{ export texts; }}\n",
        include_str!("../../orb-macro/runtime-fixture/consumed/deps/texts.wit")
    );
    let service = Contract::from_source("texts.wit", &wit, Some("provider"))
        .unwrap()
        .plan(BUILDERS, 1, &BTreeMap::new())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new(BUILDERS, 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .payload_method(
                "[constructor]builder",
                |_, _, payload: Payload| async move {
                    let (text,): (String,) = serde_json::from_value(payload.into_data()?).unwrap();
                    let mut result = Payload::data(Value::Null);
                    let index = result.attach(Issued::new(Builder {
                        nominal: Nominal {
                            interface: TEXTS.into(),
                            name: "builder".into(),
                        },
                        text: RefCell::new(text),
                    }));
                    result.map_value(|_| Ok(json!(index)))
                },
            )
            .payload_method(
                "[method]builder.append",
                |_, _, mut payload: Payload| async move {
                    let builder = payload.take_receiver().expect("the builder appended to");
                    let (word,): (String,) = serde_json::from_value(payload.into_data()?).unwrap();
                    let length = builder
                        .with_backend::<Builder, _>(|builder| {
                            let mut text = builder.text.borrow_mut();
                            text.push_str(&word);
                            text.len() as u32
                        })
                        .expect("a builder of this provider");
                    Ok(Payload::data(json!(length)))
                },
            ),
    )
}

/// Drive the speller of the Orb `name`, whose `spell-job` takes a record of the module holding an
/// imported interface's handle, as the runtime moves such a handle anywhere into an argument: a
/// builder the native provider issues goes into the call inside the record, attached to the call's
/// payload where the record's field names it; the guest appends the word through the import and
/// returns the text's length, and the builder, consumed, is closed before the call returns.
/// `spell` starts a builder of the guest's own through the import and drops it with the call.
fn spell(pool: &mut LocalPool, host: &Host, name: &str) {
    let reference = |pool: &mut LocalPool, service: String| {
        let reference = Rc::new(host.get(&service, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        reference
    };
    let speller = reference(pool, format!("{name}::example:runtime/speller@1.0.0"));
    let builders = reference(pool, format!("{BUILDERS}::{TEXTS}"));
    let call = |pool: &mut LocalPool, method: &'static str, payload: Payload| -> Value {
        let speller = speller.clone();
        pool.run_until(host.root("alice", move |context| {
            async move { speller.call_portable(&context, method, payload).await }.boxed_local()
        }))
        .unwrap()
    };
    let before = CLOSED.get();
    let builder: Owned = {
        let builders = builders.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                builders
                    .call_portable(&context, "[constructor]builder", ("hello".to_owned(),))
                    .await
            }
            .boxed_local()
        }))
        .unwrap()
    };
    assert_eq!(CLOSED.get(), before, "{name}");
    let mut payload = Payload::data(json!([{"builder": 0, "word": "world"}]));
    payload.attach_argument(builder).unwrap();
    assert_eq!(call(pool, "spell-job", payload), json!(10), "{name}");
    assert_eq!(
        CLOSED.get(),
        before + 1,
        "{name}: the guest consumed the builder"
    );
    assert_eq!(
        call(pool, "spell", Payload::data(json!(["hi"]))),
        json!(2),
        "{name}"
    );
    assert_eq!(
        CLOSED.get(),
        before + 2,
        "{name}: the guest dropped its own builder"
    );
    builders.release().unwrap();
    speller.release().unwrap();
}

/// Drive the lending of the Orb `name`, which reaches the shelves of the native keeper through the
/// view the macro derived for the fixture's keyed import of the interface the fixture implements:
/// the installation binds the view alone, and a book of the shelf's export crosses the view as the
/// import's. The books of one call go to one shelf, which counts them; a key the keeper refuses
/// comes back as the error of the WIT result.
fn lend(pool: &mut LocalPool, host: &Host, name: &str) {
    let lending = Rc::new(
        host.get(
            &format!("{name}::example:runtime/lending@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(lending.ready()).unwrap();
    let book = |title: &str, pages: u32, topic: &str| json!({"title": title, "pages": pages, "topic": topic});
    let lend = |pool: &mut LocalPool, key: &str, books: Value| {
        counting_call(pool, host, &lending, "alice", "lend", json!([key, books]))
    };
    assert_eq!(
        lend(
            pool,
            "a",
            json!([book("dune", 412, "fiction"), book("cosmos", 365, "science")])
        ),
        json!({"tag": "ok", "val": 2}),
        "{name}"
    );
    assert_eq!(
        lend(pool, "b", json!([book("emma", 474, "fiction")])),
        json!({"tag": "ok", "val": 1}),
        "{name}"
    );
    let refused = lend(pool, "refused", json!([book("dune", 412, "fiction")]));
    assert_eq!(refused["tag"], "err", "{name}: {refused}");
    assert!(
        refused["val"]
            .as_str()
            .is_some_and(|reason| reason.contains("unapproved key")),
        "{name}: {refused}"
    );
    lending.release().unwrap();
}

/// Drive the reviews of the Orb `name`: a case with named fields crosses the boundary both ways as
/// a variant case whose payload is the record of the case, `{tag, val}` with the record as its
/// value on the portable boundary, like any case with a payload: alone, in a record of the module,
/// in a list, in an option and in a result.
fn review(pool: &mut LocalPool, host: &Host, name: &str) {
    let reviews = Rc::new(
        host.get(
            &format!("{name}::example:runtime/reviews@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reviews.ready()).unwrap();
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &reviews, "alice", method, arguments)
    };
    let refused = json!({"tag": "refused", "val": {"reason": "empty", "severity": 3}});
    let deferred = json!({"tag": "deferred", "val": 2});
    let accepted = json!({"tag": "accepted"});
    assert_eq!(
        call(pool, "decide", json!(["ada", ""])),
        json!({"reviewer": "ada", "outcome": refused}),
        "{name}"
    );
    assert_eq!(
        call(pool, "decide", json!(["bob", "hi"])),
        json!({"reviewer": "bob", "outcome": deferred}),
        "{name}"
    );
    assert_eq!(
        call(pool, "decide", json!(["cy", "long enough"])),
        json!({"reviewer": "cy", "outcome": accepted}),
        "{name}"
    );
    let decisions = json!([
        {"reviewer": "ada", "outcome": refused},
        {"reviewer": "bob", "outcome": deferred},
        {"reviewer": "dee", "outcome": {"tag": "refused", "val": {"reason": "off topic", "severity": 1}}},
    ]);
    assert_eq!(
        call(pool, "reasons", json!([decisions])),
        json!(["ada: empty (3)", "dee: off topic (1)"]),
        "{name}"
    );
    assert_eq!(
        call(pool, "worst", json!([[accepted, deferred, refused], null])),
        json!({"tag": "ok", "val": refused}),
        "{name}"
    );
    assert_eq!(
        call(pool, "worst", json!([[], deferred])),
        json!({"tag": "ok", "val": deferred}),
        "{name}"
    );
    assert_eq!(
        call(pool, "worst", json!([[], null])),
        json!({"tag": "err", "val": "no outcome"}),
        "{name}"
    );
    reviews.release().unwrap();
}

/// Drive the documents of the Orb `name`: free JSON, `serde_json::Value`, crosses the boundary
/// both ways as a WIT `string` holding its JSON text, alone, in a list, in an option and in a
/// record of the module. Each value crosses as its text, which the component reads into a
/// `serde_json::Value` and writes back, its object's keys in order.
fn document(pool: &mut LocalPool, host: &Host, name: &str) {
    let documents = Rc::new(
        host.get(
            &format!("{name}::example:runtime/documents@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(documents.ready()).unwrap();
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        counting_call(pool, host, &documents, "alice", method, arguments)
    };
    assert_eq!(
        call(
            pool,
            "annotated",
            json!([r#"{"tags":["a","b"],"count":2}"#, "seen"])
        ),
        json!(r#"{"count":2,"note":"seen","tags":["a","b"]}"#),
        "{name}"
    );
    assert_eq!(
        call(pool, "annotated", json!(["[1,2]", "seen"])),
        json!("[1,2]"),
        "{name}"
    );
    assert_eq!(
        call(pool, "notes", json!([[r#"{"k":true}"#, "null"]])),
        json!([
            {"title": "0", "body": r#"{"k":true}"#},
            {"title": "1", "body": "null"},
        ]),
        "{name}"
    );
    assert_eq!(
        call(
            pool,
            "object",
            json!([{"title": "t", "body": r#"{"k":1}"#}])
        ),
        json!(r#"{"k":1}"#),
        "{name}"
    );
    assert_eq!(
        call(pool, "object", json!([{"title": "t", "body": "[1]"}])),
        Value::Null,
        "{name}"
    );
    documents.release().unwrap();
}

/// Text that holds no JSON makes the component's method panic, since reading it into the author's
/// `serde_json::Value` has no error channel there, and the panic traps the component, as any panic
/// in a component method does. The caller sees a component trap, `unavailable`, and the revision
/// is retired for every caller until a reload installs the Orb again: the same object's next call
/// fails, whatever its JSON, and so does another principal's object, which no longer activates.
/// After the reload both are served. A native Orb fails only the call, as its arguments.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn text_without_json_traps_the_component_until_a_reload() {
    let bytes = component("TEMBRA_WASM_ORB_MACRO_STD");
    let (_, contract) = contract(&bytes);
    let engine = Engine::new(Limits::default()).unwrap();
    let load = || {
        let roles = contract.embedded_roles(&bytes).unwrap();
        let artifact = contract
            .component_artifact(&bytes, "standard", 1, &roles)
            .unwrap();
        engine.load(&artifact, bound(granted())).unwrap()
    };
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        load(),
        counters(),
        builders(),
        shelves(),
        ledger(&contract),
    ])
    .unwrap();
    let service = "standard::example:runtime/documents@1.0.0";
    let annotated =
        |pool: &mut LocalPool, reference: &Rc<Reference>, principal: &'static str, text: &str| {
            let reference = reference.clone();
            let arguments = json!([text, "seen"]);
            pool.run_until(host.root(principal, move |context| {
                async move {
                    reference
                        .call_portable::<_, Value>(&context, "annotated", arguments)
                        .await
                }
                .boxed_local()
            }))
        };
    let trapped = |error: tembra::Error, reason: &str| {
        assert_eq!(
            (error.code, error.reason.as_str()),
            (Code::Unavailable, reason),
            "{error}"
        );
        assert!(error.message.starts_with("component trap"), "{error}");
    };
    let alice = Rc::new(host.get(service, vec!["main".into()], "alice").unwrap());
    pool.run_until(alice.ready()).unwrap();
    assert_eq!(
        annotated(&mut pool, &alice, "alice", "{}").unwrap(),
        json!(r#"{"note":"seen"}"#)
    );
    trapped(
        annotated(&mut pool, &alice, "alice", "{").unwrap_err(),
        "wasm_executor",
    );
    trapped(
        annotated(&mut pool, &alice, "alice", "{}").unwrap_err(),
        "wasm_executor_quarantined",
    );
    let bob = Rc::new(host.get(service, vec!["other".into()], "bob").unwrap());
    trapped(
        pool.run_until(bob.ready()).unwrap_err(),
        "wasm_executor_quarantined",
    );
    let _ = bob.release();
    pool.run_until(host.settled());
    pool.run_until(host.reload(vec![load()], 5000)).unwrap();
    assert_eq!(
        annotated(&mut pool, &alice, "alice", "[1]").unwrap(),
        json!("[1]")
    );
    let bob = Rc::new(host.get(service, vec!["other".into()], "bob").unwrap());
    pool.run_until(bob.ready()).unwrap();
    assert_eq!(
        annotated(&mut pool, &bob, "bob", "{}").unwrap(),
        json!(r#"{"note":"seen"}"#)
    );
    bob.release().unwrap();
    alice.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Drive the units of the Orb `name`: a newtype is the WIT type of its field, so each crosses as
/// its field, alone, in a list, in a tuple and in a record of the module, which the macro keeps
/// the author's, and comes back from the newtype's `TryFrom` the same.
fn measure(pool: &mut LocalPool, host: &Host, name: &str) {
    let units = Rc::new(
        host.get(
            &format!("{name}::example:runtime/units@1.0.0"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(units.ready()).unwrap();
    for (method, arguments, result) in [
        (
            "measured",
            json!([2.5, "m"]),
            json!({"amount": 2.5, "unit": "m"}),
        ),
        (
            "tally",
            json!([
                [{"amount": 1.0, "unit": "m"}, {"amount": 2.0, "unit": "s"}],
                "m"
            ]),
            json!([1, ["s"]]),
        ),
    ] {
        assert_eq!(
            counting_call(pool, host, &units, "alice", method, arguments),
            result,
            "{name} {method}"
        );
    }
    units.release().unwrap();
}

/// A newtype that only a refusing `TryFrom` makes is native only: a component has no error channel
/// for an argument refused before its method runs, only a trap, which would retire its revision for
/// every caller until a reload, so it reads a newtype through `From`. A component build of a module
/// taking such a newtype fails, and the error names the newtype, at its declaration, and says why.
/// The runtime fixture's `native-only-newtype` feature adds such a module; the build shares the
/// fixture's target directory, where `just test-orb-macro` built its components.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_component_build_refuses_a_newtype_only_try_from_makes() {
    let built = std::env::var_os("TEMBRA_WASM_ORB_MACRO_STD")
        .unwrap_or_else(|| panic!("TEMBRA_WASM_ORB_MACRO_STD is required"));
    let target = Path::new(&built)
        .parent()
        .expect("the fixture's target directory");
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture/Cargo.toml");
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--features",
            "native-only-newtype",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target)
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{errors}");
    for expected in [
        "error[E0277]: `Symbol` is native only: a component reads it from its field through `From<std::string::String>`, which `Symbol` does not implement",
        "pub struct Symbol(String);",
        "a newtype a component takes, which only `TryFrom` makes",
        "a component has no error channel for an argument refused before its method runs, only a trap, which would retire its revision for every caller until a reload",
    ] {
        assert!(errors.contains(expected), "{expected}\n{errors}");
    }
}

/// A build for wasm32 is a component whatever the facade's features. A guest gets `host` when its
/// `tembra` keeps the default features, or from another crate of the build; its component keeps the
/// bindings' `context` all the same, and answers each call as the guest built without `host` does,
/// where the native `context` would find no call to read and trap the revision. A `no_std` guest
/// with `host`, or with `contracts` alone, links as a component too, on the panic handler of the
/// std the facade links. The
/// host feature fixture is built both ways, std and without, into the fixtures' target directory,
/// where `just test-orb-macro` built their components.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_wasm32_guest_with_the_host_feature_answers_as_a_component() {
    let built = std::env::var_os("TEMBRA_WASM_ORB_MACRO_STD")
        .unwrap_or_else(|| panic!("TEMBRA_WASM_ORB_MACRO_STD is required"));
    let target = Path::new(&built)
        .parent()
        .expect("the fixtures' target directory");
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/host-feature-fixture/Cargo.toml");
    let answers = [
        &[][..],
        &["--features", "host"],
        &["--features", "no-std"],
        &["--features", "no-std,host"],
        &["--features", "no-std,contracts"],
    ]
    .map(|features| {
        let output = std::process::Command::new(env!("CARGO"))
            .args([
                "rustc",
                "--locked",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--crate-type",
                "cdylib",
            ])
            .args(features)
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--target-dir")
            .arg(target)
            .args(["--", "-Clinker=wasm-component-ld"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{features:?}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(
            target.join("wasm32-unknown-unknown/release/tembra_orb_host_feature_fixture.wasm"),
        )
        .unwrap();
        let contract =
            Contract::from_source("probe.wit", &embedded_wit(&bytes), Some("probe")).unwrap();
        let artifact = contract
            .component_artifact(&bytes, "probe", 1, &Default::default())
            .unwrap();
        let engine = Engine::new(Limits::default()).unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(&artifact, Install::default()).unwrap()])
            .unwrap();
        let caller = Rc::new(
            host.get(
                "probe::example:probe/caller@1.0.0",
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        );
        pool.run_until(caller.ready()).unwrap();
        // A trap would fail the first call and retire the revision for the second.
        let answers = [(); 2].map(|()| {
            let caller = caller.clone();
            pool.run_until(host.root("alice", move |context| {
                async move {
                    caller
                        .call_portable::<_, Value>(&context, "principal", json!([]))
                        .await
                }
                .boxed_local()
            }))
            .map_err(|error| (error.code, error.reason))
        });
        caller.release().unwrap();
        pool.run_until(host.close()).unwrap();
        answers
    });
    assert_eq!(answers[0], [Ok(json!("alice")), Ok(json!("alice"))]);
    assert_eq!(answers[1], answers[0], "built with `host`");
    assert_eq!(answers[2], answers[0], "built without std");
    assert_eq!(answers[3], answers[0], "built without std, with `host`");
    assert_eq!(
        answers[4], answers[0],
        "built without std, with `contracts`"
    );
}

/// The fixtures' target directory, where `just test-orb-macro` built their components, and the
/// manifest of the runtime fixture.
fn runtime_fixture() -> (PathBuf, PathBuf) {
    let built = std::env::var_os("TEMBRA_WASM_ORB_MACRO_STD")
        .unwrap_or_else(|| panic!("TEMBRA_WASM_ORB_MACRO_STD is required"));
    let target = Path::new(&built)
        .parent()
        .expect("the fixtures' target directory")
        .to_owned();
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture/Cargo.toml");
    (target, manifest)
}

/// Run the native tests of the runtime fixture's module that `features` select: the crate built
/// for the host with the facade's `host`, by the name the module gives the facade, whose tests
/// install the module's `orb()` in a native host and call it. The build shares the fixtures'
/// target directory.
fn module_natively(features: &str) {
    let (target, manifest) = runtime_fixture();
    // The module of `renamed` names the facade `sdk`.
    let native = if features.starts_with("renamed") {
        "native-sdk"
    } else {
        "native"
    };
    let output = std::process::Command::new(env!("CARGO"))
        .args(["test", "--locked", "--lib", "--no-default-features"])
        .arg("--features")
        .arg(format!("{features},{native}"))
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{features}\n{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !report.contains("running 0 tests"),
        "{features}: no native test\n{report}"
    );
}

/// The runtime fixture built as the component of another of its Orb modules, which `features`
/// select in place of the default `guest`, with `no-std` for the freestanding runtime. The build
/// shares the fixtures' target directory, where `just test-orb-macro` built their components, and
/// one build runs at a time, since each writes the same file.
fn module_component(features: &str) -> Vec<u8> {
    static BUILD: Mutex<()> = Mutex::new(());
    let _alone = BUILD.lock().unwrap_or_else(PoisonError::into_inner);
    let (target, manifest) = runtime_fixture();
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "rustc",
            "--locked",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--no-default-features",
            "--features",
            features,
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .args(["--", "-Clinker=wasm-component-ld"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{features}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read(target.join("wasm32-unknown-unknown/release/tembra_orb_runtime_fixture.wasm"))
        .unwrap()
}

/// The contract of a module's component as `cargo tembra build` writes it, from the WIT the macro
/// embedded and the packages of the directory the module consumes, and the artifact of the Orb
/// `name`, packaged with the roles the component carries.
fn module_artifact(component: &[u8], name: &str) -> (ModuleContract, Vec<u8>) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture");
    let consumed = tembra_contracts::EmbeddedManifest::read(component)
        .unwrap()
        .wit
        .map(|dir| fixture.join(dir));
    let module = ModuleContract::from_component(component, consumed.as_deref())
        .unwrap()
        .expect("the component carries the WIT of its module");
    let contract = module.contract();
    let roles = contract.embedded_roles(component).unwrap();
    let artifact = contract
        .component_artifact(component, name, 1, &roles)
        .unwrap();
    (module, artifact)
}

/// The contract of a module's component, as `module_artifact` reads it, and the Orb `name` an
/// engine loads from it under an installation that grants nothing.
fn module_orb(engine: &Engine, component: &[u8], name: &str) -> (ModuleContract, Orb) {
    let (module, artifact) = module_artifact(component, name);
    (module, engine.load(&artifact, Install::default()).unwrap())
}

/// The WIT of a contract without its whitespace, for the checks of its declarations.
fn words(module: &ModuleContract) -> String {
    module.text().split_whitespace().collect()
}

/// A reference to the `main` object of the service `name` of `alice`, once it is ready.
fn ready(pool: &mut LocalPool, host: &Host, name: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(name, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Names that are keywords of WIT, in a component build, std and freestanding, and natively: the
/// module `list` of the `keywords` feature is the world `%list`, whose contract `cargo tembra
/// build` writes to `contract/%list.wit`, its method `record` is `%record` and its field `type_`
/// is `%type`. The component answers the method under its WIT name, and the field crosses both
/// ways, `type` in the boundary's JSON; so does the module's `orb()` in the fixture's native test.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn names_that_are_wit_keywords_answer_both_ways() {
    let engine = Engine::new(Limits::default()).unwrap();
    for features in ["keywords", "keywords,no-std"] {
        let (module, orb) = module_orb(&engine, &module_component(features), "keywords");
        assert_eq!(module.world(), "%list", "{features}");
        let (words, text) = (words(&module), module.text());
        for expected in [
            "world%list{exportwords;}",
            "recordmarks{bold:bool,%type:bool,}",
            "%record:func(word:string,marks:marks)->marks;",
        ] {
            assert!(words.contains(expected), "{features}: {expected}\n{text}");
        }
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let reference = ready(&mut pool, &host, "keywords::example:keywords/words@1.0.0");
        let mut record = |arguments: Value| {
            counting_call(&mut pool, &host, &reference, "alice", "record", arguments)
        };
        assert_eq!(
            record(json!(["List", {"bold": true, "type": false}])),
            json!({"bold": true, "type": true}),
            "{features}"
        );
        assert_eq!(
            record(json!(["list", {"bold": false, "type": true}])),
            json!({"bold": false, "type": true}),
            "{features}"
        );
        assert_eq!(
            record(json!(["list", {"bold": false, "type": false}])),
            json!({"bold": false, "type": false}),
            "{features}"
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("keywords");
}

/// The author's types, in a component build, std and freestanding, and natively: the module
/// `requests` of the `authored` feature declares a record in a sub-module with a private field, a
/// field `r#type` and a case `HTTP`, which the bindings spell `type_` and `Http`, and a function's
/// error enum with a hand `impl Display`. Each stays the author's type, converted at the boundary:
/// the private field crosses as `seen`, the field as `type`, the case as `http`, and the refusal's
/// words come from the author's `Display`; so does the module's `orb()` in the fixture's native
/// test.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn the_authors_types_answer_both_ways() {
    let engine = Engine::new(Limits::default()).unwrap();
    for features in ["authored", "authored,no-std"] {
        let (module, orb) = module_orb(&engine, &module_component(features), "authored");
        let (words, text) = (words(&module), module.text());
        for expected in [
            "recordrequest{path:string,%type:scheme,seen:u32,}",
            "enumscheme{http,https,}",
            "recordrefusal-too-long{length:u32,}",
            "recordrefusal-stale{seen:u32,}",
            "variantrefusal{empty,too-long(refusal-too-long),stale(refusal-stale),}",
            "fresh:func(path:string)->request;",
            "route:func(request:request)->result<request,refusal>;",
            "explain:func(refusal:refusal)->string;",
        ] {
            assert!(words.contains(expected), "{features}: {expected}\n{text}");
        }
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let reference = ready(&mut pool, &host, "authored::example:authored/router@1.0.0");
        let mut route = |method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, &reference, "alice", method, arguments)
        };
        assert_eq!(
            route(
                "route",
                json!([{"path": "secure", "type": "http", "seen": 2}])
            ),
            json!({"tag": "ok", "val": {"path": "secure", "type": "https", "seen": 3}}),
            "{features}"
        );
        assert_eq!(
            route(
                "route",
                json!([{"path": "plain", "type": "http", "seen": 0}])
            ),
            json!({"tag": "ok", "val": {"path": "plain", "type": "http", "seen": 1}}),
            "{features}"
        );
        assert_eq!(
            route("route", json!([{"path": "", "type": "https", "seen": 0}])),
            json!({"tag": "err", "val": {"tag": "empty"}}),
            "{features}"
        );
        assert_eq!(
            route(
                "route",
                json!([{"path": "much-too-long", "type": "https", "seen": 0}])
            ),
            json!({"tag": "err", "val": {"tag": "too-long", "val": {"length": 13}}}),
            "{features}"
        );
        assert_eq!(
            route(
                "explain",
                json!([{"tag": "too-long", "val": {"length": 13}}])
            ),
            json!("the path has 13 characters"),
            "{features}"
        );
        assert_eq!(
            route("explain", json!([{"tag": "empty"}])),
            json!("the path is empty"),
            "{features}"
        );
        assert_eq!(
            route("fresh", json!(["new"])),
            json!({"path": "new", "type": "http", "seen": 0}),
            "{features}"
        );
        assert_eq!(
            route("route", json!([{"path": "old", "type": "http", "seen": 3}])),
            json!({"tag": "err", "val": {"tag": "stale", "val": {"seen": 3}}}),
            "{features}"
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("authored");
}

/// A native Orb contributing to the series hook `hook` of the Orb `provider`, at priority 0: it
/// notes each input in `seen`.
fn observer(contract: &Contract, provider: &str, hook: &str, seen: Rc<RefCell<Vec<Value>>>) -> Orb {
    let signature = contract.function_signature(hook, "apply", true).unwrap();
    let series = tembra::Hook::<Value, ()>::series(hook).signature(signature);
    Orb::new("observer", 1)
        .requires(tembra::core::Requirement {
            orb: provider.into(),
            api_version: 1,
            methods: Default::default(),
            hooks: [hook.into()].into(),
            signatures: Default::default(),
            hook_signatures: Default::default(),
        })
        .contribute(provider, &series, 0, move |_, input: Value| {
            seen.borrow_mut().push(input);
            async { Ok(()) }
        })
}

/// Items under `#[cfg]` in a component build, std and freestanding, and natively: the module
/// `notebook` of the `configured` feature keeps notes, which it clears without the crate's
/// `archive` feature and archives with it, onto the shelf of the service `Shelf`, whose struct and
/// block the feature adds, telling the contributors to the hook a sub-module the feature adds
/// declares. The component, its WIT, the hooks its roles declare and the contract `cargo tembra
/// build` writes from it hold the services, methods and hooks of the build's features, and the
/// component answers them. Natively, `orb()` serves the notes without the feature, and refuses
/// the hook's declaration with it, as the native side refuses any.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn items_under_cfg_follow_the_features_both_ways() {
    const NOTES: &str = "notes::example:notes/notes@1.0.0";
    const SHELF: &str = "notes::example:notes/shelf@1.0.0";
    const ARCHIVED: &str = "example:notes/archived@1.0.0";
    let engine = Engine::new(Limits::default()).unwrap();
    for (features, archive) in [
        ("configured", false),
        ("configured,archive", true),
        ("configured,no-std", false),
        ("configured,archive,no-std", true),
    ] {
        let component = module_component(features);
        let (module, artifact) = module_artifact(&component, "notes");
        assert_eq!(module.world(), "notebook", "{features}");
        let (words, text) = (words(&module), module.text());
        for (declaration, held) in [
            ("add:func(text:string)->u32;", true),
            ("clear:func()->u32;", !archive),
            ("archive:func()->u32;", archive),
            ("interfaceshelf{", archive),
            ("count:func()->u32;", archive),
            ("exportshelf;", archive),
            ("interfacearchived{", archive),
            ("apply:func(input:u32);", archive),
            ("importarchived;", archive),
            ("exportarchived;", archive),
        ] {
            assert_eq!(
                words.contains(declaration),
                held,
                "{features}: {declaration}\n{text}"
            );
        }
        let roles = module.contract().embedded_roles(&component).unwrap();
        let declared: Vec<&String> = roles.hooks.keys().collect();
        let expected: &[&str] = if archive { &[ARCHIVED] } else { &[] };
        assert_eq!(declared, expected, "{features}");
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut orbs = vec![engine.load(&artifact, Install::default()).unwrap()];
        if archive {
            orbs.push(observer(module.contract(), "notes", ARCHIVED, seen.clone()));
        }
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(orbs).unwrap();
        let notes = ready(&mut pool, &host, NOTES);
        let mut call = |reference: &Rc<Reference>, method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, reference, "alice", method, arguments)
        };
        assert_eq!(call(&notes, "add", json!(["one"])), json!(1), "{features}");
        assert_eq!(call(&notes, "add", json!(["two"])), json!(2), "{features}");
        if archive {
            assert_eq!(call(&notes, "archive", json!([])), json!(2), "{features}");
            assert_eq!(*seen.borrow(), [json!(2)], "{features}");
            let shelf = Rc::new(host.get(SHELF, vec!["main".into()], "alice").unwrap());
            assert_eq!(call(&shelf, "count", json!([])), json!(2), "{features}");
            shelf.release().unwrap();
        } else {
            assert_eq!(call(&notes, "clear", json!([])), json!(2), "{features}");
            let shelf = host.get(SHELF, vec!["main".into()], "alice");
            assert!(shelf.is_err(), "{features}: a shelf without the archive");
        }
        assert_eq!(
            call(&notes, "add", json!(["three"])),
            json!(1),
            "{features}"
        );
        notes.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("configured");
    module_natively("configured,archive");
}

/// `crate = ".."` on a module whose `#[cfg]` items make the macro expand it again, in a component
/// build, std and freestanding, and natively: the module `tally` of the `renamed` feature, whose
/// crate names the facade `sdk` and nothing else, has a method under the crate's `archive`
/// feature. Each copy of the module goes through `::sdk::orb`, and the component reaches its state
/// and the call's `context` through the facade by that name, so it builds with and without the
/// feature, and answers; so does the module's `orb()` in the fixture's native test, built with
/// the facade's `host` under that name.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_renamed_facade_serves_the_copies_both_ways() {
    let engine = Engine::new(Limits::default()).unwrap();
    for (features, archive) in [
        ("renamed", false),
        ("renamed,archive", true),
        ("renamed,no-std", false),
        ("renamed,archive,no-std", true),
    ] {
        let (module, orb) = module_orb(&engine, &module_component(features), "renamed");
        assert_eq!(module.world(), "tally", "{features}");
        let (words, text) = (words(&module), module.text());
        assert_eq!(
            words.contains("reset:func()->u32;"),
            archive,
            "{features}\n{text}"
        );
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let counter = ready(&mut pool, &host, "renamed::example:renamed/counter@1.0.0");
        let state = counter.state::<Value>().unwrap();
        let mut call = |method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, &counter, "alice", method, arguments)
        };
        assert_eq!(
            call("add", json!([2])),
            json!("alice counts 2"),
            "{features}"
        );
        assert_eq!(
            call("add", json!([3])),
            json!("alice counts 5"),
            "{features}"
        );
        assert_eq!(*state.value().unwrap(), json!({"total": 5}), "{features}");
        if archive {
            assert_eq!(call("reset", json!([])), json!(5), "{features}");
            assert_eq!(*state.value().unwrap(), json!({"total": 0}), "{features}");
        }
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("renamed");
    module_natively("renamed,archive");
}

/// The hooks the gatehouse declares, its desk, and the Orb it is installed as.
const VISITS: &str = "example:gatehouse/visits@1.0.0";
const SPOTS: &str = "example:gatehouse/spots@1.0.0";
const DOORS: &str = "example:gatehouse/doors@1.0.0";
const ROOMS: &str = "example:gatehouse/rooms@1.0.0";
const GUARD: &str = "example:admission/guard@1.0.0";
const DESK: &str = "gatehouse::example:gatehouse/desk@1.0.0";

/// A native Orb contributing twice to each hook of the gatehouse, at the priorities -1 and 1:
/// both see each visit and each spot, which they note in `seen`; the first answers a room above the ground floor
/// only and the second any floor; the first adds one to an admission and the second blocks one
/// above ten.
fn contributions(contract: &Contract, seen: Rc<RefCell<Vec<(i32, Value)>>>) -> Orb {
    let signature = |hook: &str| contract.function_signature(hook, "apply", true).unwrap();
    let visits = tembra::Hook::<Value, ()>::series(VISITS).signature(signature(VISITS));
    let spots = tembra::Hook::<Value, ()>::series(SPOTS).signature(signature(SPOTS));
    let doors = tembra::Hook::<Value, ()>::series(DOORS).signature(signature(DOORS));
    let rooms = tembra::Hook::<u32, Option<u32>>::bail(ROOMS).signature(signature(ROOMS));
    let guard = tembra::Hook::<Value, Value>::waterfall(GUARD).signature(signature(GUARD));
    let mut watch = Orb::new("watch", 1).requires(tembra::core::Requirement {
        orb: "gatehouse".into(),
        api_version: 1,
        methods: Default::default(),
        hooks: [
            VISITS.into(),
            SPOTS.into(),
            DOORS.into(),
            ROOMS.into(),
            GUARD.into(),
        ]
        .into(),
        signatures: Default::default(),
        hook_signatures: Default::default(),
    });
    for priority in [-1, 1] {
        let seen = seen.clone();
        watch = watch.contribute("gatehouse", &visits, priority, move |_, visit: Value| {
            seen.borrow_mut().push((priority, visit));
            async { Ok(()) }
        });
    }
    for priority in [-1, 1] {
        let seen = seen.clone();
        watch = watch.contribute("gatehouse", &spots, priority, move |_, spot: Value| {
            seen.borrow_mut().push((priority, spot));
            async { Ok(()) }
        });
    }
    for priority in [-1, 1] {
        let seen = seen.clone();
        watch = watch.contribute("gatehouse", &doors, priority, move |_, door: Value| {
            seen.borrow_mut().push((priority, door));
            async { Ok(()) }
        });
    }
    watch
        .contribute("gatehouse", &rooms, -1, |_, floor: u32| async move {
            Ok((floor > 0).then_some(floor * 100))
        })
        .contribute("gatehouse", &rooms, 1, |_, floor: u32| async move {
            Ok(Some(floor))
        })
        .contribute("gatehouse", &guard, -1, |_, admission: Value| async move {
            let value = admission["value"].as_i64().unwrap() + 1;
            Ok(json!({"value": value, "blocked": admission["blocked"]}))
        })
        .contribute("gatehouse", &guard, 1, |_, admission: Value| async move {
            let value = admission["value"].as_i64().unwrap();
            Ok(json!({"value": value, "blocked": value > 10}))
        })
}

/// Hook declarations of each composition, in a component build, std and freestanding, on the
/// compiled and on the interpreted engine: the module `gatehouse` of the `hooks` feature declares
/// a series whose input holds a newtype, a series whose input is a newtype holding a record of the
/// module, a series in a module of no imports whose input is a newtype of a `String` declared in
/// another, and a bail from their input, and the waterfall of a consumed package by its identity,
/// which the component's roles name with their compositions, and whose admission a function of the
/// desk takes and returns as the hook's own type. Native contributions answer them in plan order:
/// every contribution to a series sees each visit, spot and door, a newtype as its field, the first
/// answer of the bail gives the room, and each contribution to the waterfall changes the admission
/// in turn. Without contributions, the series end, the bail answers nothing
/// and the waterfall hands its input back. Natively, the module compiles, and its `orb()` refuses
/// the declarations, which only a component serves.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn hooks_a_component_declares_compose_their_contributions() {
    for engine in [
        Engine::new(Limits::default()).unwrap(),
        Engine::interpreted(Limits::default()).unwrap(),
    ] {
        compose_the_declared_hooks(&engine);
        assert_eq!(engine.instances(), 0);
    }
    module_natively("hooks");
}

/// Load the gatehouse into `engine`, std and freestanding, with and without contributions, and
/// run each of its hooks (`hooks_a_component_declares_compose_their_contributions`).
fn compose_the_declared_hooks(engine: &Engine) {
    for features in ["hooks", "hooks,no-std"] {
        let component = module_component(features);
        let (module, artifact) = module_artifact(&component, "gatehouse");
        let roles = module.contract().embedded_roles(&component).unwrap();
        assert_eq!(
            roles.hooks,
            BTreeMap::from([
                (
                    GUARD.to_owned(),
                    tembra_contracts::HookComposition::Waterfall
                ),
                (DOORS.to_owned(), tembra_contracts::HookComposition::Series),
                (ROOMS.to_owned(), tembra_contracts::HookComposition::Bail),
                (SPOTS.to_owned(), tembra_contracts::HookComposition::Series),
                (VISITS.to_owned(), tembra_contracts::HookComposition::Series),
            ]),
            "{features}"
        );
        let seen = Rc::new(RefCell::new(Vec::new()));
        for contributed in [false, true] {
            let mut orbs = vec![engine.load(&artifact, Install::default()).unwrap()];
            if contributed {
                orbs.push(contributions(module.contract(), seen.clone()));
            }
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            host.install(orbs).unwrap();
            let desk = ready(&mut pool, &host, DESK);
            let mut call = |method: &'static str, arguments: Value| {
                counting_call(&mut pool, &host, &desk, "alice", method, arguments)
            };
            assert_eq!(call("visit", json!(["ada", 9])), Value::Null, "{features}");
            assert_eq!(call("go", json!([3, "east"])), Value::Null, "{features}");
            assert_eq!(call("knock", json!(["east"])), Value::Null, "{features}");
            let rooms = [0, 3].map(|floor| call("room", json!([floor])));
            let admitted = [5, 10].map(|value| call("admit", json!([value])));
            let admissions =
                [5, 10].map(|value| call("admitted", json!([{"value": value, "blocked": false}])));
            let visit = json!({"who": "ada", "hour": 9});
            let spot = json!({"floor": 3, "door": "east"});
            let door = json!("east");
            if contributed {
                assert_eq!(
                    *seen.borrow(),
                    [
                        (-1, visit.clone()),
                        (1, visit),
                        (-1, spot.clone()),
                        (1, spot),
                        (-1, door.clone()),
                        (1, door)
                    ],
                    "{features}"
                );
                assert_eq!(rooms, [json!(0), json!(300)], "{features}");
                assert_eq!(
                    admitted,
                    [json!([6, false]), json!([11, true])],
                    "{features}"
                );
                assert_eq!(
                    admissions,
                    [
                        json!({"value": 6, "blocked": false}),
                        json!({"value": 11, "blocked": true})
                    ],
                    "{features}"
                );
            } else {
                assert!(seen.borrow().is_empty(), "{features}");
                assert_eq!(rooms, [Value::Null, Value::Null], "{features}");
                assert_eq!(
                    admitted,
                    [json!([5, false]), json!([10, false])],
                    "{features}"
                );
                assert_eq!(
                    admissions,
                    [
                        json!({"value": 5, "blocked": false}),
                        json!({"value": 10, "blocked": false})
                    ],
                    "{features}"
                );
            }
            desk.release().unwrap();
            pool.run_until(host.close()).unwrap();
            assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
        }
    }
}

/// A module that contributes to a hook builds whatever the runtime's features: as a component, std
/// and freestanding, and natively with the facade's `host` alone, where its `orb(..)` answers an
/// error naming the runtime's `hooks`, and with `hooks`, where its contribution joins the hook of
/// a native gate. The module is `contributor` of the runtime fixture.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_contribution_builds_with_or_without_the_runtimes_hooks() {
    for features in ["contributor", "contributor,no-std"] {
        let component = module_component(features);
        module_artifact(&component, "contributor");
    }
    module_natively("contributor");
    module_natively("contributor,native-hooks");
}

/// A state's version across a reload, in a component build, std and freestanding: the counter of
/// the `versioned` feature keeps its state at version 1, and its next revision, built with
/// `revised`, at version 2. The versions are compared on reload. Changing the version of a living
/// object's state needs a migration of its value, which the macro does not declare: the reload is
/// refused with `invalid/state_version`, as a native host refuses it, and the running revision
/// keeps serving the object. Once the object is released the reload goes through, and a new
/// object starts from the next revision's state at its version; going back to version 1 is then
/// refused the same way.
/// Natively, each revision keeps its state at its version, across a reload of its own code: one
/// build of the crate holds one revision, so a native host cannot reload one into the other.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_state_version_is_compared_across_a_reload() {
    const COUNTER: &str = "tallies::example:versioned/counter@1.0.0";
    let engine = Engine::new(Limits::default()).unwrap();
    for runtime in ["", ",no-std"] {
        let [first, next] = ["versioned", "versioned,revised"].map(|features| {
            module_artifact(
                &module_component(&format!("{features}{runtime}")),
                "tallies",
            )
            .1
        });
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(&first, Install::default()).unwrap()])
            .unwrap();
        let reload = |pool: &mut LocalPool, artifact: &[u8]| {
            let orb = engine.load(artifact, Install::default()).unwrap();
            pool.run_until(host.reload(vec![orb], 5_000)).map(|_| ())
        };
        let counter = ready(&mut pool, &host, COUNTER);
        let add = |pool: &mut LocalPool, counter: &Rc<Reference>, by: u32| {
            counting_call(pool, &host, counter, "alice", "add", json!([by]))
        };
        assert_eq!(add(&mut pool, &counter, 3), json!(3), "{runtime}");
        let refused = reload(&mut pool, &next).expect_err("a raised version needs a migration");
        assert_eq!(
            (refused.code, refused.reason.as_str()),
            (Code::Invalid, "state_version"),
            "{runtime}: {refused}"
        );
        assert_eq!(add(&mut pool, &counter, 1), json!(4), "{runtime}");
        assert_eq!(counter.state_snapshot().unwrap().0.version, 1, "{runtime}");
        counter.release().unwrap();
        pool.run_until(host.settled());
        assert_eq!(reload(&mut pool, &next), Ok(()), "{runtime}");
        let counter = ready(&mut pool, &host, COUNTER);
        let state = counter.state::<Value>().unwrap();
        assert_eq!(
            *state.value().unwrap(),
            json!({"total": 0, "additions": 0}),
            "{runtime}"
        );
        assert_eq!(add(&mut pool, &counter, 2), json!(2), "{runtime}");
        assert_eq!(
            *state.value().unwrap(),
            json!({"total": 2, "additions": 1}),
            "{runtime}"
        );
        assert_eq!(counter.state_snapshot().unwrap().0.version, 2, "{runtime}");
        let refused = reload(&mut pool, &first).expect_err("a lowered version needs a migration");
        assert_eq!(
            (refused.code, refused.reason.as_str()),
            (Code::Invalid, "state_version"),
            "{runtime}: {refused}"
        );
        assert_eq!(add(&mut pool, &counter, 1), json!(3), "{runtime}");
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{runtime}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("versioned");
    module_natively("versioned,revised");
}

/// A resource of the Orb's root, in a component build, std and freestanding, and natively: the
/// lamp of the `lamps` feature, at version 1, opens in the component when the Orb's root is
/// created and its `#[close]` function runs at teardown, each writing to the host's log. A reload
/// to the next revision, whose lamp is at version 2, opens that lamp in the new instance and
/// closes the first one, lit as it was; closing the host closes the second. Natively, the host
/// opens each revision's lamp at its version, keeps it across a reload of the same code, and
/// closes it through its `#[close]` function when it closes.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_root_resource_opens_and_closes_both_ways() {
    const SWITCH: &str = "lamps::example:lamps/switch@1.0.0";
    let records = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    let logged = || std::mem::take(&mut *records.lock().unwrap());
    for runtime in ["", ",no-std"] {
        let [first, next] = ["lamps", "lamps,revised"].map(|features| {
            module_artifact(&module_component(&format!("{features}{runtime}")), "lamps").1
        });
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(&first, Install::default()).unwrap()])
            .unwrap();
        let switch = ready(&mut pool, &host, SWITCH);
        let light = |pool: &mut LocalPool, switch: &Rc<Reference>| {
            counting_call(pool, &host, switch, "alice", "light", json!([]))
        };
        assert_eq!(light(&mut pool, &switch), json!(1), "{runtime}");
        assert_eq!(light(&mut pool, &switch), json!(2), "{runtime}");
        assert_eq!(logged(), ["open 1"], "{runtime}");
        switch.release().unwrap();
        let orb = engine.load(&next, Install::default()).unwrap();
        pool.run_until(host.reload(vec![orb], 5_000)).unwrap();
        pool.run_until(host.settled());
        let switch = ready(&mut pool, &host, SWITCH);
        assert_eq!(light(&mut pool, &switch), json!(1), "{runtime}");
        assert_eq!(logged(), ["open 2", "close 1, lit 2"], "{runtime}");
        switch.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(logged(), ["close 2, lit 1"], "{runtime}");
        assert!(host.errors().is_empty(), "{runtime}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("lamps");
    module_natively("lamps,revised");
}

/// A root resource that depends on another, in a component build, std and freestanding, and
/// natively: the cache of the `cellar` feature, `#[resource(depends = [Db])]`, comes first in its
/// module, yet the component declares it after the database with the database as its dependency,
/// so the kernel opens the database first, whose rows the cache's `#[open]` function reads, and
/// closes the cache first, whose `#[close]` function writes them back to the database.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_root_resource_opens_after_its_dependency_both_ways() {
    let records = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    let logged = || std::mem::take(&mut *records.lock().unwrap());
    for features in ["cellar", "cellar,no-std"] {
        let (_, orb) = module_orb(&engine, &module_component(features), "cellar");
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let shelf = ready(&mut pool, &host, "cellar::example:cellar/shelf@1.0.0");
        assert_eq!(
            counting_call(&mut pool, &host, &shelf, "alice", "rows", json!([])),
            json!(3),
            "{features}"
        );
        assert_eq!(
            logged(),
            ["open db at [] of host", "open cache, 3 rows"],
            "{features}"
        );
        shelf.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(
            logged(),
            ["close cache, 6 rows", "close db, 6 rows"],
            "{features}"
        );
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("cellar");
}

/// An `#[open]` that returns `Result`, in a component build, std and freestanding, and natively:
/// in the `kiln` feature the wood opens through `Ok`, and the fire, which depends on it, answers
/// `Err`, so the activation fails as it does for a native Orb whose resource fails to open, and
/// the kernel closes the wood. The object waiting on the activation fails with the opener's
/// error, the same both ways: its code, reason and the error's `Display` text.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn an_open_that_answers_an_error_fails_the_activation_both_ways() {
    let records = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    let logged = || std::mem::take(&mut *records.lock().unwrap());
    for features in ["kiln", "kiln,no-std"] {
        let (_, orb) = module_orb(&engine, &module_component(features), "kiln");
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let hearth = host
            .get(
                "kiln::example:kiln/hearth@1.0.0",
                vec!["main".into()],
                "alice",
            )
            .unwrap();
        let refused = pool
            .run_until(hearth.ready())
            .expect_err("the activation fails");
        assert_eq!(
            (
                refused.code,
                refused.reason.as_str(),
                refused.message.as_str()
            ),
            (
                Code::Unavailable,
                "resource",
                "the resource `fire` did not open: 2 damp logs"
            ),
            "{features}: {refused}"
        );
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
        assert_eq!(logged(), ["open wood", "close wood"], "{features}");
    }
    assert_eq!(engine.instances(), 0);
    module_natively("kiln");
}

/// The resources of a service's objects, in a component build, std and freestanding, and natively:
/// in the `dock` feature each `Ship` object owns a mooring, `#[resource(depends = [Harbor])]` in the
/// service's module, and a line, `#[resource(depends = [Mooring])]`. The kernel opens them when it
/// creates the ship, after the root's harbor, the mooring's `#[open]` function taking the harbor's
/// next berth and the line's reading the mooring's berth, both with `with`, and closes them in
/// reverse when the ship goes, the mooring's `#[close]` function giving the berth back to the
/// harbor. Each ship reads its own, so two ships moor at two berths and tie their own lines; the
/// third ship's mooring takes berth 3, where no line reaches, so its line answers `Err`, which
/// fails that ship's creation with `unavailable/resource` and the opener's text, and the kernel
/// closes the mooring it opened. A ship owns a state beside its resources, which `sail` updates,
/// and hands out a WIT resource, its manifest, whose method reads the ship's mooring. Natively the
/// module's own test checks the same.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_service_resource_opens_and_closes_per_object_both_ways() {
    let records = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    let logged = || std::mem::take(&mut *records.lock().unwrap());
    for features in ["dock", "dock,no-std"] {
        let (_, orb) = module_orb(&engine, &module_component(features), "dock");
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let ship = |key: &str| {
            Rc::new(
                host.get("dock::example:dock/ship@1.0.0", vec![key.into()], "alice")
                    .unwrap(),
            )
        };
        let one = ship("one");
        pool.run_until(one.ready()).unwrap();
        let two = ship("two");
        pool.run_until(two.ready()).unwrap();
        assert_eq!(
            logged(),
            [
                "open harbor",
                "open mooring 1 at [one] of alice",
                "open line at berth 1",
                "open mooring 2 at [two] of alice",
                "open line at berth 2",
            ],
            "{features}"
        );
        let answer = |pool: &mut LocalPool, ship: &Rc<Reference>, method, arguments| {
            counting_call(pool, &host, ship, "alice", method, arguments)
        };
        assert_eq!(
            answer(&mut pool, &one, "berth", json!([])),
            json!(1),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &two, "berth", json!([])),
            json!(2),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &one, "tie", json!([3])),
            json!(3),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &one, "tie", json!([2])),
            json!(5),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &two, "tie", json!([1])),
            json!(1),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &one, "sail", json!([])),
            json!([1, 5]),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &one, "sail", json!([])),
            json!([2, 5]),
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &two, "sail", json!([])),
            json!([1, 1]),
            "{features}"
        );
        let manifest = two.clone();
        let described: String = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    let manifest: Owned = manifest
                        .call_portable(&context, "[constructor]manifest", ("cargo".to_owned(),))
                        .await?;
                    manifest
                        .call_portable(&context, "[method]manifest.describe", json!([]))
                        .await
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(described, "cargo at berth 2", "{features}");
        let three = ship("three");
        let refused = pool
            .run_until(three.ready())
            .expect_err("the third ship's line reaches no berth");
        assert_eq!(
            (
                refused.code,
                refused.reason.as_str(),
                refused.message.as_str()
            ),
            (
                Code::Unavailable,
                "resource",
                "the resource `line` did not open: no line reaches berth 3"
            ),
            "{features}: {refused}"
        );
        assert_eq!(
            logged(),
            [
                "open mooring 3 at [three] of alice",
                "close mooring 3, 2 moored"
            ],
            "{features}"
        );
        one.release().unwrap();
        pool.run_until(host.settled());
        assert_eq!(
            logged(),
            ["close line at berth 1", "close mooring 1, 1 moored"],
            "{features}"
        );
        assert_eq!(
            answer(&mut pool, &two, "tie", json!([1])),
            json!(2),
            "{features}"
        );
        two.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(
            logged(),
            [
                "close line at berth 2",
                "close mooring 2, 0 moored",
                "close harbor, 3 issued",
            ],
            "{features}"
        );
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("dock");
}

/// The resources of a service's objects in an Orb whose root owns none, in a component build, std
/// and freestanding, and natively: in the `quay` feature each `Barge` object owns a fender, which
/// the kernel opens when it creates the barge and closes when the barge goes, and each barge
/// counts its own bumps in it. The world exports `service-resources-init` without
/// `resources-init`.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_service_resource_opens_without_a_root_resource_both_ways() {
    let records = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    let logged = || std::mem::take(&mut *records.lock().unwrap());
    for features in ["quay", "quay,no-std"] {
        let (module, orb) = module_orb(&engine, &module_component(features), "quay");
        assert!(
            words(&module).contains("worldquay{importtembra:guest/resources@1.0.0;exporttembra:guest/service-resources-init@1.0.0;importtembra:guest/context@1.0.0;exportbarge;}"),
            "{features}: {}",
            module.text()
        );
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let barge = |pool: &mut LocalPool, key: &str| {
            let barge = Rc::new(
                host.get("quay::example:quay/barge@1.0.0", vec![key.into()], "alice")
                    .unwrap(),
            );
            pool.run_until(barge.ready()).unwrap();
            barge
        };
        let one = barge(&mut pool, "one");
        let two = barge(&mut pool, "two");
        assert_eq!(logged(), ["open fender", "open fender"], "{features}");
        let bump = |pool: &mut LocalPool, barge: &Rc<Reference>| {
            counting_call(pool, &host, barge, "alice", "bump", json!([]))
        };
        assert_eq!(bump(&mut pool, &one), json!(1), "{features}");
        assert_eq!(bump(&mut pool, &one), json!(2), "{features}");
        assert_eq!(bump(&mut pool, &two), json!(1), "{features}");
        one.release().unwrap();
        pool.run_until(host.settled());
        assert_eq!(logged(), ["close fender, 2 bumps"], "{features}");
        two.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(logged(), ["close fender, 1 bumps"], "{features}");
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("quay");
}

/// An interface with `flags` consumed by a guest that depends on the facade alone, in a component
/// build, std and freestanding: the module `holder` of the `flagged` feature implements
/// `example:rights/rights@1.0.0`, whose `access` the bindings generate without `wit-bindgen`'s
/// `bitflags` feature, and a derived service takes and returns the same type. A value crosses as
/// an object of its set flags. The module's native test runs from the native fixture's
/// `tests/flags.rs`.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_consumed_flags_crosses_a_component_with_the_facade_alone() {
    const RIGHTS: &str = "holder::example:rights/rights@1.0.0";
    const CHECKS: &str = "holder::example:holder/checks@1.0.0";
    let engine = Engine::new(Limits::default()).unwrap();
    for features in ["flagged", "flagged,no-std"] {
        let (module, orb) = module_orb(&engine, &module_component(features), "holder");
        let (words, text) = (words(&module), module.text());
        assert!(
            words.contains("missing:func(access:access,wanted:access)->access;"),
            "{features}\n{text}"
        );
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let (rights, checks) = (
            ready(&mut pool, &host, RIGHTS),
            ready(&mut pool, &host, CHECKS),
        );
        let mut call = |reference: &Rc<Reference>, method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, reference, "alice", method, arguments)
        };
        assert_eq!(
            call(&rights, "invert", json!([{"read": true}])),
            json!({"write": true, "share": true}),
            "{features}"
        );
        let grants = json!([
            {"who": "ada", "access": {"read": true}},
            {"who": "bob", "access": {"share": true}},
        ]);
        assert_eq!(
            call(&rights, "merged", json!([grants])),
            json!({"read": true, "share": true}),
            "{features}"
        );
        assert_eq!(
            call(
                &checks,
                "missing",
                json!([{"read": true}, {"read": true, "write": true}])
            ),
            json!({"write": true}),
            "{features}"
        );
        rights.release().unwrap();
        checks.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
}

/// The installation of a WASI module, with the grants `system` gives it, refused: the kernel
/// answers `forbidden/orb_grant`, naming the interface the grants leave out, `missing`.
fn refused_without(engine: &Engine, artifact: &[u8], install: Install, missing: &str) {
    let Err(error) = engine.load(artifact, install) else {
        panic!("installed without the grant of {missing}");
    };
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Forbidden, "orb_grant"),
        "{error}"
    );
    assert!(error.message.contains(missing), "{missing}: {error}");
}

/// WASI imports of the host's system capabilities besides the random source and the monotonic
/// clock, in a component build, std and freestanding: the module `system` of the `system` feature
/// reads the system clock, a duration of the clocks' types, the CLI's arguments and environment
/// and the insecure random source, under an installation that grants `clocks`, `cli` and
/// `random`. The kernel refuses one that grants no `cli`, naming the CLI's interface. Natively,
/// the module compiles, and its `orb()` refuses the imports, which only a component serves.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn wasi_imports_answer_in_a_component_under_their_grants() {
    const PROBE: &str = "system::example:system/probe@1.0.0";
    let engine = Engine::new(Limits::default()).unwrap();
    let granted = |system: &[SystemCapability]| {
        let mut install = Install::default();
        install.grants.system = system.iter().copied().collect();
        install.cli.arguments = vec!["probe".into()];
        install.cli.environment = vec![("MODE".into(), "test".into())];
        install
    };
    for features in ["system", "system,no-std"] {
        let (_, artifact) = module_artifact(&module_component(features), "system");
        let partly = granted(&[SystemCapability::Clocks, SystemCapability::Random]);
        refused_without(&engine, &artifact, partly, "wasi:cli/");
        let all = [
            SystemCapability::Clocks,
            SystemCapability::Cli,
            SystemCapability::Random,
        ];
        let orb = engine.load(&artifact, granted(&all)).unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let probe = ready(&mut pool, &host, PROBE);
        let mut call = |method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, &probe, "alice", method, arguments)
        };
        let seconds = call("seconds", json!([])).as_i64().unwrap();
        assert!(seconds > 1_700_000_000, "{features}: {seconds}");
        assert_eq!(
            call("nanoseconds", json!([2])),
            json!(2_000_000_000_u64),
            "{features}"
        );
        assert_eq!(
            call("environment", json!([])),
            json!(["probe", "MODE=test"]),
            "{features}"
        );
        assert_eq!(call("insecure", json!([8])), json!(8), "{features}");
        probe.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("system");
}

/// Accept one connection on the loopback address and answer `response` once its request head has
/// arrived, on a thread of its own; return the port.
fn peer(response: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut socket, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !response.is_empty() && !head.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        socket.write_all(response.as_bytes()).unwrap();
    });
    port
}

/// WASI imports of the providers a host configures, in a component build, std and freestanding:
/// the module `io` of the `io` feature lists the preopened directories, a type of the filesystem's
/// types, which it reaches by the bindings' path since it imports the sockets' `types`, connects
/// over TCP through a socket of those types, and sends an HTTP request through the client. Each
/// needs the grant of a configured provider: a filesystem, a network destination and an HTTP
/// origin; the kernel refuses an installation that grants no network, naming the sockets'
/// interface. Natively, the module compiles, and its `orb()` refuses the imports.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn wasi_provider_imports_answer_in_a_component_under_their_grants() {
    const OUTSIDE: &str = "io::example:io/outside@1.0.0";
    let directory = tempfile::tempdir().unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .filesystem("data", directory.path())
        .unwrap()
        .network("net")
        .unwrap();
    for features in ["io", "io,no-std"] {
        let (_, artifact) = module_artifact(&module_component(features), "io");
        let (listening, serving) = (
            peer(""),
            peer("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello"),
        );
        let granted = |network: bool| {
            let mut install = Install::default();
            install.grants.filesystem = vec![tembra_executor_wasm::FilesystemGrant {
                provider: "data".into(),
                mount: "/data".into(),
                writable: false,
            }];
            install.grants.http = vec![tembra_contracts::HttpGrant {
                provider: "net".into(),
                origins: vec![format!("http://127.0.0.1:{serving}")],
            }];
            if network {
                install.grants.network = vec![tembra_contracts::NetworkGrant {
                    provider: "net".into(),
                    connect: vec![([127, 0, 0, 1], listening).into()],
                    ..Default::default()
                }];
            }
            install
        };
        refused_without(&engine, &artifact, granted(false), "wasi:sockets/");
        let orb = engine.load(&artifact, granted(true)).unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let outside = ready(&mut pool, &host, OUTSIDE);
        let mut call = |method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, &outside, "alice", method, arguments)
        };
        assert_eq!(
            call("directories", json!([])),
            json!(["/data: directory"]),
            "{features}"
        );
        assert_eq!(
            call("connect", json!([listening])),
            json!({"tag": "ok"}),
            "{features}"
        );
        let authority = format!("127.0.0.1:{serving}");
        assert_eq!(
            call("fetch", json!([authority, "/"])),
            json!({"tag": "ok", "val": "200 hello"}),
            "{features}"
        );
        outside.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("io");
}

/// A module with a state, a root resource and the `context` that implements an interface of a
/// contract whose `deps/` holds the kernel's package, `tembra-guest.wit`, as `cargo tembra build`
/// writes it, in a component build, std and freestanding, and natively: the module `steady` of the
/// `steady` feature consumes the state example's contract. The macro reads the kernel's package
/// once, from that directory, and the component answers: each object counts in its state, through
/// the root's resource, and the `context` names the call's principal, which the counter refuses
/// for `mallory`; so does the module's `orb()` in the fixture's native test.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD"]
fn a_contract_holding_the_kernel_package_is_consumed_both_ways() {
    const COUNTER: &str = "steady::example:counter/counter@1.0.0";
    let engine = Engine::new(Limits::default()).unwrap();
    for features in ["steady", "steady,no-std"] {
        let (_, orb) = module_orb(&engine, &module_component(features), "steady");
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let counter = ready(&mut pool, &host, COUNTER);
        let mut call = |principal: &str, method: &'static str, arguments: Value| {
            counting_call(&mut pool, &host, &counter, principal, method, arguments)
        };
        assert_eq!(
            call("alice", "add", json!([2])),
            json!({"tag": "ok", "val": {"total": 2, "steps": 1}}),
            "{features}"
        );
        assert_eq!(
            call("alice", "add", json!([3])),
            json!({"tag": "ok", "val": {"total": 5, "steps": 2}}),
            "{features}"
        );
        assert_eq!(
            call("mallory", "add", json!([1])),
            json!({"tag": "err", "val": "mallory may not count"}),
            "{features}"
        );
        assert_eq!(
            call("alice", "read", json!([])),
            json!({"total": 5, "steps": 2}),
            "{features}"
        );
        assert_eq!(
            *counter.state::<Value>().unwrap().value().unwrap(),
            json!({"count": {"total": 5, "steps": 2}}),
            "{features}"
        );
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{features}: {:?}", host.errors());
    }
    assert_eq!(engine.instances(), 0);
    module_natively("steady");
}
