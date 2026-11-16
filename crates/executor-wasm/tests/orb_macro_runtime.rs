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

#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn std_and_freestanding_macro_guests_share_a_contract_and_execute() {
    let standard = component("TEMBRA_WASM_ORB_MACRO_STD");
    let freestanding = component("TEMBRA_WASM_ORB_MACRO_NO_STD");
    let (std_wit, std_contract) = contract(&standard);
    let (freestanding_wit, freestanding_contract) = contract(&freestanding);
    assert_eq!(std_wit, freestanding_wit);
    assert_eq!(std_contract.model(), freestanding_contract.model());
    // The kernel serves the walker's state, the host chance's WASI imports, and an Orb provider
    // answers the keyed import's view and the plain import of the same interface.
    let bindings = BTreeMap::from([
        ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
        host("wasi:random/random@0.3.0", "random"),
        host("wasi:clocks/monotonic-clock@0.3.0", "clocks"),
        view(),
        provider(),
    ]);
    assert_eq!(
        std_contract.plan("runtime", 1, &bindings).unwrap().orb,
        freestanding_contract
            .plan("runtime", 1, &bindings)
            .unwrap()
            .orb
    );

    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let orbs = [
            ("standard", &standard, &std_contract),
            ("freestanding", &freestanding, &freestanding_contract),
        ]
        .into_iter()
        .map(|(name, bytes, contract)| {
            let roles = contract.embedded_roles(bytes).unwrap();
            let artifact = contract.component_artifact(bytes, name, 1, &roles).unwrap();
            engine.load(&artifact, bound(granted())).unwrap()
        })
        .chain([counters()])
        .collect();
        host.install(orbs).unwrap();
        for name in ["standard", "freestanding"] {
            let reference = Rc::new(
                host.get(
                    &format!("{name}::example:runtime/greeting@1.0.0"),
                    vec!["main".into()],
                    "alice",
                )
                .unwrap(),
            );
            pool.run_until(reference.ready()).unwrap();
            for name in ["Ada", "a longer name with spaces", "🦀"] {
                let call = reference.clone();
                let result = pool
                    .run_until(host.root("alice", move |context| {
                        async move {
                            call.call_portable::<_, String>(&context, "hello", (name.to_owned(),))
                                .await
                        }
                        .boxed_local()
                    }))
                    .unwrap();
                assert_eq!(result, format!("Hello, {name}!"));
            }
            reference.release().unwrap();
            walk(&mut pool, &host, name);
        }
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        let diagnostics = host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert!(host.errors().is_empty());
    }
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
        let walker = walker.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                walker
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
            }
            .boxed_local()
        }))
        .unwrap()
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

/// Chance calls WASI through the interfaces its attribute imports, with std and without: the
/// contract depends on the bundled packages it imports from, which `cargo tembra build` writes into
/// `contract/deps/`, and binds each import to the host's provider of its grant, as for any compiled
/// component; the kernel refuses an installation that does not grant both; and a granted one draws
/// from the engine's secure random source and waits on the host's monotonic clock, on both engines.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn wasi_imports_reach_the_host_under_the_installations_grants() {
    const RANDOM: &str = "wasi:random/random@0.3.0";
    const CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.0";
    for variable in ["TEMBRA_WASM_ORB_MACRO_STD", "TEMBRA_WASM_ORB_MACRO_NO_STD"] {
        let bytes = component(variable);
        let (_, contract) = contract(&bytes);
        let dependencies: Vec<&str> = contract
            .standard_dependencies()
            .into_iter()
            .map(|(file, _)| file)
            .collect();
        assert_eq!(
            dependencies,
            ["wasi-clocks.wit", "wasi-random.wit", "tembra-guest.wit"],
            "{variable}"
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
        let bindings = contract.providers(&requires, &granted().grants).unwrap();
        assert_eq!(
            bindings,
            BTreeMap::from([
                ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
                host(RANDOM, "random"),
                host(CLOCK, "clocks"),
                view(),
                provider(),
            ]),
            "{variable}"
        );
        let plan = contract.plan("runtime", 1, &bindings).unwrap();
        let mut imports: Vec<(&str, &str)> = plan
            .host_imports
            .iter()
            .map(|import| (import.contract.as_str(), import.provider.as_str()))
            .collect();
        imports.sort();
        assert_eq!(
            imports,
            [(CLOCK, "clocks"), (RANDOM, "random")],
            "{variable}"
        );
        // No WASI import requires an Orb: the provider of the view and of the plain import is the
        // only one.
        let required: Vec<&str> = plan
            .orb
            .requires
            .iter()
            .map(|requirement| requirement.orb.as_str())
            .collect();
        assert_eq!(required, [COUNTERS], "{variable}");
        let roles = contract.embedded_roles(&bytes).unwrap();
        let artifact = contract
            .component_artifact(&bytes, "runtime", 1, &roles)
            .unwrap();
        for interpreted in [false, true] {
            let engine = if interpreted {
                Engine::interpreted(Limits::default())
            } else {
                Engine::new(Limits::default())
            }
            .unwrap()
            .secure_random(|| Deterministic::new(vec![42]));
            for (system, missing) in [
                (vec![], ""),
                (vec![SystemCapability::Clocks], RANDOM),
                (vec![SystemCapability::Random], CLOCK),
                (vec![SystemCapability::Cli], ""),
            ] {
                let mut install = bound(Install::default());
                install.grants.system = system.iter().copied().collect();
                let Err(error) = engine.load(&artifact, install) else {
                    panic!("{variable}: installed with only {system:?}");
                };
                assert_eq!(
                    (error.code, error.reason.as_str()),
                    (Code::Forbidden, "orb_grant"),
                    "{variable}: {error}"
                );
                assert!(error.message.contains(missing), "{variable}: {error}");
            }
            assert_eq!(
                engine.instances(),
                0,
                "{variable}: a refused Orb runs nothing"
            );
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            host.install(vec![
                counters(),
                engine.load(&artifact, bound(granted())).unwrap(),
            ])
            .unwrap();
            let chance = Rc::new(
                host.get(
                    "runtime::example:runtime/chance@1.0.0",
                    vec!["main".into()],
                    "alice",
                )
                .unwrap(),
            );
            pool.run_until(chance.ready()).unwrap();
            let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
                let chance = chance.clone();
                pool.run_until(host.root("alice", move |context| {
                    async move {
                        chance
                            .call_portable::<_, Value>(&context, method, arguments)
                            .await
                    }
                    .boxed_local()
                }))
                .unwrap()
            };
            assert_eq!(
                call(&mut pool, "draw", json!([5])),
                json!([42, 42, 42, 42, 42])
            );
            assert_eq!(call(&mut pool, "draw", json!([0])), json!([]));
            // The call suspends on the host's clock and resumes when the wait is over.
            let waited = call(&mut pool, "pause", json!([2_000_000]))
                .as_u64()
                .unwrap();
            assert!(waited >= 2_000_000, "{variable}: waited {waited} ns");
            chance.release().unwrap();
            pool.run_until(host.close()).unwrap();
            assert_eq!(engine.instances(), 0);
            assert!(host.errors().is_empty());
        }
    }
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

/// Call `method` of the fixture's `counting` service in a root of `principal`.
fn counting_call(
    pool: &mut LocalPool,
    host: &Host,
    counting: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> Value {
    let counting = counting.clone();
    pool.run_until(host.root(principal, move |context| {
        async move {
            counting
                .call_portable::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
    .unwrap()
}

/// The `counting` service of both guests reaches the objects of a native provider through the
/// view the macro derived for the fixture's keyed import, packaged with the role the component
/// carries. In one call, two references of one key reach one object and another key's object
/// counts on its own; each principal's calls reach objects of its own; a key the provider refuses
/// comes back as the error of the WIT result, and the revision serves the next call. The
/// provider's record, which the view's module names, crosses `counting` as it is.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn a_keyed_import_reaches_the_object_of_each_key() {
    let guests = [
        ("standard", component("TEMBRA_WASM_ORB_MACRO_STD")),
        ("freestanding", component("TEMBRA_WASM_ORB_MACRO_NO_STD")),
    ];
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let mut orbs = vec![counters()];
        for (name, bytes) in &guests {
            let (_, contract) = contract(bytes);
            let roles = contract.embedded_roles(bytes).unwrap();
            assert_eq!(
                roles.references,
                BTreeMap::from([(COUNTER_VIEW.to_owned(), COUNTER.to_owned())])
            );
            let artifact = contract.component_artifact(bytes, name, 1, &roles).unwrap();
            let decoded = tembra_contracts::Artifact::decode(&artifact).unwrap();
            assert_eq!(
                decoded.contract.references()[COUNTER_VIEW].interface,
                COUNTER
            );
            orbs.push(engine.load(&artifact, bound(granted())).unwrap());
        }
        host.install(orbs).unwrap();
        for (name, _) in &guests {
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
                        &mut pool,
                        &host,
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
                &mut pool,
                &host,
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
                    &mut pool,
                    &host,
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
                counting_call(&mut pool, &host, &counting, "bob", "read", json!(["d"])),
                json!({"tag": "ok", "val": {"key": ["d"], "principal": "bob", "count": 0}}),
                "{name}"
            );
            counting.release().unwrap();
        }
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        let diagnostics = host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert!(host.errors().is_empty());
    }
}

/// A derived function takes back the resources its interface hands out, on both guests and both
/// engines: the caller's handle moves into the call and the guest consumes the value.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn derived_functions_consume_the_resources_they_take() {
    let guests = [
        ("standard", component("TEMBRA_WASM_ORB_MACRO_STD")),
        ("freestanding", component("TEMBRA_WASM_ORB_MACRO_NO_STD")),
    ];
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let orbs = guests
            .iter()
            .map(|(name, bytes)| {
                let (_, contract) = contract(bytes);
                let roles = contract.embedded_roles(bytes).unwrap();
                let artifact = contract.component_artifact(bytes, name, 1, &roles).unwrap();
                engine.load(&artifact, bound(granted())).unwrap()
            })
            .chain([counters()])
            .collect();
        host.install(orbs).unwrap();
        for (name, _) in &guests {
            tally(&mut pool, &host, &engine, name);
        }
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
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

/// A flags value, the author's struct of `bool` fields, crosses the boundary both ways on both
/// guests and both engines: as a parameter and a result, inside a record, a list and an option, and
/// in a state the kernel keeps as JSON.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn flags_cross_the_boundary_both_ways() {
    let guests = [
        ("standard", component("TEMBRA_WASM_ORB_MACRO_STD")),
        ("freestanding", component("TEMBRA_WASM_ORB_MACRO_NO_STD")),
    ];
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let orbs = guests
            .iter()
            .map(|(name, bytes)| {
                let (wit, contract) = contract(bytes);
                let words: String = wit.split_whitespace().collect();
                assert!(words.contains("flagsaccess{read,write,share,}"), "{wit}");
                let roles = contract.embedded_roles(bytes).unwrap();
                let artifact = contract.component_artifact(bytes, name, 1, &roles).unwrap();
                engine.load(&artifact, bound(granted())).unwrap()
            })
            .chain([counters()])
            .collect();
        host.install(orbs).unwrap();
        for (name, _) in &guests {
            permit(&mut pool, &host, name);
        }
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}

/// Drive the permits of the Orb `name`. A flags value is an object of its set flags on the
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
        let permits = permits.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                permits
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
            }
            .boxed_local()
        }))
        .unwrap()
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

/// The records of the consumed packages cross the derived interfaces of both guests, on both
/// engines, as the consumed packages define them. `readings` `use`s the tally of the imported
/// interface: the tally the plain import reads comes back alone and in a record of the module, goes
/// back in as a parameter, alone and in a list, and the kernel keeps the last one in the readings
/// object's state, as the JSON `serde` writes for the provider's record. `library` `use`s the book
/// and the topic of the shelf the Orb implements, whose export defines them.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_MACRO_STD and TEMBRA_WASM_ORB_MACRO_NO_STD"]
fn consumed_records_cross_a_derived_interface_as_they_are() {
    let guests = [
        ("standard", component("TEMBRA_WASM_ORB_MACRO_STD")),
        ("freestanding", component("TEMBRA_WASM_ORB_MACRO_NO_STD")),
    ];
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let orbs = guests
            .iter()
            .map(|(name, bytes)| {
                let (wit, contract) = contract(bytes);
                let words: String = wit.split_whitespace().collect();
                for expected in [
                    "interfacereadings{useexample:counters/counter@1.0.0.{tally};",
                    "interfacelibrary{useexample:shelves/shelf@1.0.0.{book,topic};",
                ] {
                    assert!(words.contains(expected), "{expected}\n{wit}");
                }
                let roles = contract.embedded_roles(bytes).unwrap();
                let artifact = contract.component_artifact(bytes, name, 1, &roles).unwrap();
                engine.load(&artifact, bound(granted())).unwrap()
            })
            .chain([counters()])
            .collect();
        host.install(orbs).unwrap();
        // Both guests reach the provider's `main` object, the host's: the second counts on.
        for ((name, _), counted) in guests.iter().zip([0, 7]) {
            read(&mut pool, &host, name, counted);
            shelve(&mut pool, &host, name);
        }
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}

/// Drive the readings of the Orb `name`, whose plain import reaches the provider's `main` object
/// after it has counted `counted`. A tally is the provider's record on the portable boundary and
/// in the state.
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
        let readings = readings.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                readings
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
            }
            .boxed_local()
        }))
        .unwrap()
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
            let reference = reference.clone();
            pool.run_until(host.root("alice", move |context| {
                async move {
                    reference
                        .call_portable::<_, Value>(&context, method, arguments)
                        .await
                }
                .boxed_local()
            }))
            .unwrap()
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
