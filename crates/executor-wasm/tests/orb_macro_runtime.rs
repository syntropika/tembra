//! Real macro guests link either std or the explicitly selected freestanding runtime.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};
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
    let nested: Vec<_> = resolve
        .packages
        .iter()
        .map(|(id, _)| id)
        .filter(|id| *id != package)
        .collect();
    printer.print(&resolve, package, &nested).unwrap();
    let text = String::from(printer.output);
    let contract =
        Contract::from_source("runtime.wit", &text, Some(&resolve.worlds[world].name)).unwrap();
    (text, contract)
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
    assert_eq!(
        std_contract
            .plan("runtime", 1, &Default::default())
            .unwrap()
            .orb,
        freestanding_contract
            .plan("runtime", 1, &Default::default())
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
            engine.load(&artifact, Install::default()).unwrap()
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
