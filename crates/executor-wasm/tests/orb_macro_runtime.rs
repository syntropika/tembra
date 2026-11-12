//! Real macro guests link either std or the explicitly selected freestanding runtime.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{collections::BTreeMap, rc::Rc};
use tembra::{Host, core::Code};
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
    // The kernel serves the walker's state, the host chance's WASI imports.
    let bindings = BTreeMap::from([
        ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
        host("wasi:random/random@0.3.0", "random"),
        host("wasi:clocks/monotonic-clock@0.3.0", "clocks"),
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
            let artifact = contract
                .component_artifact(bytes, name, 1, &Default::default())
                .unwrap();
            engine.load(&artifact, granted()).unwrap()
        })
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
        let bindings = contract
            .providers(&BTreeMap::new(), &granted().grants)
            .unwrap();
        assert_eq!(
            bindings,
            BTreeMap::from([
                ("tembra:guest/state@1.0.0".to_owned(), Binding::Kernel),
                host(RANDOM, "random"),
                host(CLOCK, "clocks"),
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
        assert!(plan.orb.requires.is_empty(), "{variable}");
        let artifact = contract
            .component_artifact(&bytes, "runtime", 1, &Default::default())
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
                let mut install = Install::default();
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
            host.install(vec![engine.load(&artifact, granted()).unwrap()])
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
