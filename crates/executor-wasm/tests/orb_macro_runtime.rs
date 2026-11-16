//! Real macro guests link either std or the explicitly selected freestanding runtime.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra::{
    Context, Host, Orb, Reference, Service,
    core::Code,
    portable::{Owned, Payload},
};
use tembra_executor_wasm::{
    Binding, Contract, Deterministic, Engine, Install, Limits, SystemCapability,
};
use wit_component::{DecodedWasm, WitPrinter};

fn component(variable: &str) -> Vec<u8> {
    let path = std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} is required"));
    std::fs::read(path).unwrap_or_else(|error| panic!("reading {variable}: {error}"))
}

fn contract(bytes: &[u8]) -> (String, Contract) {
    let DecodedWasm::Component(resolve, world) = wit_component::decode(bytes).unwrap() else {
        panic!("the macro fixture must be a component");
    };
    let package = resolve.worlds[world].package.unwrap();
    let mut printer = WitPrinter::default();
    // The kernel's package, which the walker's state imports from, is bundled with the contracts,
    // and so are the WASI packages chance imports from.
    let nested: Vec<_> = resolve
        .packages
        .iter()
        .filter(|(id, nested)| {
            *id != package
                && nested.name.namespace != "wasi"
                && (nested.name.namespace != "tembra" || nested.name.name != "guest")
        })
        .map(|(id, _)| id)
        .collect();
    printer.print(&resolve, package, &nested).unwrap();
    let text = String::from(printer.output);
    let contract =
        Contract::from_source("runtime.wit", &text, Some(&resolve.worlds[world].name)).unwrap();
    (text, contract)
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
/// WASI imports, and an Orb provider answers the keyed import's view and the plain import of the
/// same interface.
fn bindings() -> BTreeMap<String, Binding> {
    BTreeMap::from([
        ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
        host(RANDOM, "random"),
        host(CLOCK, "clocks"),
        view(),
        provider(),
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
/// as for any compiled component, and requires one Orb, the provider of the keyed import's view
/// and of the plain import. The WIT holds the author's flags and `use`s the consumed packages'
/// records as they define them: `readings` the tally of the imported interface, `library` the
/// book and the topic of the shelf the Orb implements. The roles the component carries pair the
/// view with the interface it views, and the artifact keeps the pair.
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
        // The keyed import's view and the plain import need their provider named; the WASI
        // imports need their grants.
        let orb = tembra_contracts::OrbProvider {
            orb: COUNTERS.into(),
            api_version: 1,
        };
        let requires = BTreeMap::from([
            (COUNTER_VIEW.to_owned(), orb.clone()),
            (COUNTER.to_owned(), orb),
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
        // No WASI import requires an Orb: the provider of the view and of the plain import is
        // the only one.
        let required: Vec<&str> = plan
            .orb
            .requires
            .iter()
            .map(|requirement| requirement.orb.as_str())
            .collect();
        assert_eq!(required, [COUNTERS], "{name}");
        let words: String = wit.split_whitespace().collect();
        for expected in [
            "flagsaccess{read,write,share,}",
            "interfacereadings{useexample:counters/counter@1.0.0.{tally};",
            "interfacelibrary{useexample:shelves/shelf@1.0.0.{book,topic};",
        ] {
            assert!(words.contains(expected), "{name}: {expected}\n{wit}");
        }
        let roles = contract.embedded_roles(&bytes).unwrap();
        assert_eq!(
            roles.references,
            BTreeMap::from([(COUNTER_VIEW.to_owned(), COUNTER.to_owned())]),
            "{name}"
        );
        let artifact = contract
            .component_artifact(&bytes, name, 1, &roles)
            .unwrap();
        let decoded = tembra_contracts::Artifact::decode(&artifact).unwrap();
        assert_eq!(
            decoded.contract.references()[COUNTER_VIEW].interface,
            COUNTER,
            "{name}"
        );
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

/// Load both guests into `engine` once, beside the native provider of the interface they import,
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
    let orbs = loaded.into_iter().chain([counters()]).collect();
    host.install(orbs).unwrap();
    for (guest, counted) in guests.iter().zip([0, 7]) {
        let name = guest.name;
        greet(&mut pool, &host, name);
        walk(&mut pool, &host, name);
        chance(&mut pool, &host, name);
        count(&mut pool, &host, name);
        tally(&mut pool, &host, &engine, name);
        permit(&mut pool, &host, name);
        read(&mut pool, &host, name, counted);
        shelve(&mut pool, &host, name);
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

/// An installation of the fixture that binds the keyed import's view and the plain import to the
/// provider; without them the kernel refuses the fixture, as for any import of another Orb.
fn bound(install: Install) -> Install {
    install
        .requires(COUNTER_VIEW, COUNTERS, 1)
        .requires(COUNTER, COUNTERS, 1)
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
