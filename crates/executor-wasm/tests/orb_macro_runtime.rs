//! Real macro guests link either std or the explicitly selected freestanding runtime.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra::{
    Context, Host, Orb, Reference, Service,
    core::Code,
    portable::{Capability, Issued, Nominal, Owned, Payload},
};
use tembra_executor_wasm::{
    Binding, Contract, Deterministic, Engine, Install, Limits, SystemCapability,
};
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
/// resource and so a provider. The WIT holds the author's flags and `use`s the consumed packages'
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
            "flagsaccess{read,write,share,}",
            "interfacereadings{useexample:counters/counter@1.0.0.{tally};",
            "interfacelibrary{useexample:shelves/shelf@1.0.0.{book,topic};",
            "interfacespeller{useexample:texts/texts@1.0.0.{builder};",
            "recordjob{builder:builder,word:string,}",
            "interfaceshelf-ref{useexample:shelves/shelf@1.0.0.{book};",
            "interfacelending{useexample:shelves/shelf@1.0.0.{book};",
            "importshelf-ref;",
            "exportexample:shelves/shelf@1.0.0;",
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
        json!("Position { at: Point { x: 0, y: 0 }, facing: Heading::North, moves: 6 }"),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"at": {"x": 0, "y": 0}, "facing": "North", "moves": 6}),
        "{name}"
    );
    walker.release().unwrap();
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
/// list and an option, or to another tally's `absorb` leaves the caller and the provider's handle
/// table, and the guest drops the value it took back.
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
    tallies.release().unwrap();
}

/// Drive the permits of the Orb `name`. A flags value, the author's struct of `bool` fields,
/// crosses the boundary both ways: as a parameter and a result, inside a record, a list and an
/// option, and in a state the kernel keeps as JSON. It is an object of its set flags on the
/// portable boundary, a flag left out unset; the state's JSON is serde's, every field of the
/// struct.
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
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": {"read": false, "write": false, "share": false}, "grants": 0}),
        "{name}"
    );
    assert_eq!(
        call(pool, "invert", json!([{"read": true, "share": true}])),
        json!({"write": true}),
        "{name}"
    );
    assert_eq!(
        call(pool, "invert", json!([{}])),
        json!({"read": true, "write": true, "share": true}),
        "{name}"
    );
    assert_eq!(
        call(pool, "give", json!(["ada", {"write": true, "share": true}])),
        json!({"who": "ada", "access": {"write": true, "share": true}}),
        "{name}"
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({"last": {"read": false, "write": true, "share": true}, "grants": 1}),
        "{name}"
    );
    assert_eq!(
        call(pool, "kept", json!([])),
        json!({"last": {"write": true, "share": true}, "grants": 1}),
        "{name}"
    );
    let grants = json!([
        {"who": "ada", "access": {"read": true}},
        {"who": "bob", "access": {"share": true}},
    ]);
    assert_eq!(
        call(pool, "merged", json!([grants, null])),
        json!({"read": true, "share": true}),
        "{name}"
    );
    assert_eq!(
        call(pool, "merged", json!([[], {"write": true}])),
        json!({"write": true}),
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
