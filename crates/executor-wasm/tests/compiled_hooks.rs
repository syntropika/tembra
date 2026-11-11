//! A real compiled callback contributes through the same typed native hook registry.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Hook, Host, Orb, Service};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const GUARD: &str = "example:compiled-hooks/guard@1.0.0";
const WIT: &str = r#"
package example:compiled-hooks@1.0.0;
interface guard { apply: func(input: u32) -> u32; }
world provider { export guard; }
world extension { import guard; export guard; }
"#;

fn artifact(calls_anchor: bool) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("hooks.wit", WIT).unwrap();
    let world = resolve.select_world(&[package], Some("extension")).unwrap();
    let module = if calls_anchor {
        r#"(module
        (import "example:compiled-hooks/guard@1.0.0" "apply" (func $anchor (param i32) (result i32)))
        (func (export "example:compiled-hooks/guard@1.0.0#apply") (param i32) (result i32)
          local.get 0 call $anchor))"#
    } else {
        r#"(module
      (func (export "example:compiled-hooks/guard@1.0.0#apply") (param i32) (result i32)
        local.get 0 i32.const 2 i32.add))"#
    };
    let mut core = wat::parse_str(module).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .unwrap()
        .encode()
        .unwrap();
    Contract::from_source("hooks.wit", WIT, Some("extension"))
        .unwrap()
        .component_artifact(
            &component,
            "guard",
            1,
            &tembra_contracts::ComponentRoles {
                contributions: [(GUARD.into(), -7)].into(),
                ..Default::default()
            },
        )
        .unwrap()
}

fn run(calls_anchor: bool) {
    let bytes = artifact(calls_anchor);
    for engine in [
        Engine::new(Limits::default()).unwrap(),
        Engine::interpreted(Limits::default()).unwrap(),
    ] {
        let contract = Contract::from_source("hooks.wit", WIT, Some("provider")).unwrap();
        let hook = Hook::<u32, u32>::waterfall(GUARD)
            .signature(contract.function_signature(GUARD, "apply", true).unwrap());
        let bound = hook.clone();
        let provider = Orb::new("tools", 1).hook(hook).service(
            Service::new("tools.run", move |declaration| declaration.hook(&bound))
                .method("run", |hook, context, (input,): (u32,)| async move {
                    hook.run(&context, input).await
                }),
        );
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![
            provider,
            engine
                .load(&bytes, Install::default().requires(GUARD, "tools", 1))
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(host.get("tools.run", vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        let called = reference.clone();
        let result: tembra::Result<u32> = pool.run_until(host.root("alice", move |context| {
            async move { called.call(&context, "run", (3_u32,)).await }.boxed_local()
        }));
        if calls_anchor {
            assert!(
                result
                    .unwrap_err()
                    .message
                    .contains("contribution type anchor")
            );
        } else {
            assert_eq!(result.unwrap(), 5);
        }
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty());
    }
}

#[test]
fn compiled_contributions_preserve_wit_types_and_share_native_composition() {
    run(false);
}

#[test]
fn a_contribution_type_anchor_never_becomes_an_ordinary_service_import() {
    run(true);
}
