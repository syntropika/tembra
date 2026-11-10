//! Real compiled, source and native participants execute waterfall, series and bail hooks.
//!
//! Plan order is compiled (-7), source (0), then native (10). Every bail input proves a branch:
//! 0 is undecided by both guests and reaches the native fallback, 1 is decided by the compiled
//! contributor before the source one could answer, and 2 continues past the compiled `none` to
//! the source `some`. Series results are WIT void, which the portable boundary carries as null.
use futures::{FutureExt, executor::LocalPool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra::{Hook, Host, Orb, Service, core::Requirement};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const GUARD: &str = "example:hooks/guard@1.0.0";
const AUDIT: &str = "example:hooks/audit@1.0.0";
const CHOICE: &str = "example:hooks/choice@1.0.0";
const RUNNER: &str = "tools::example:hooks/runner@1.0.0";
const SOURCE_REPORTER: &str = "extension::example:hooks/reporter@1.0.0";
const COMPILED_REPORTER: &str = "compiled::example:hooks/reporter@1.0.0";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct Admission {
    value: i32,
    blocked: bool,
}

/// Native participant invocations: hook and input.
type Calls = Rc<RefCell<Vec<(&'static str, i32)>>>;

fn contract(world: &str) -> Contract {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/hooks"),
        Some(world),
    )
    .unwrap()
}

/// Contributors import each hook interface from the provider named `tools`.
fn requires() -> Install {
    Install::default()
        .requires(GUARD, "tools", 1)
        .requires(AUDIT, "tools", 1)
        .requires(CHOICE, "tools", 1)
}

/// A source contributor whose contributions update a resource owned by its Orb root.
///
/// Managed state belongs to one service object, so the Orb root keeps its observations in a
/// root-owned resource; the reporter service reads that resource through `Declaration.orb`.
fn source_extension(engine: &Engine) -> Orb {
    let source = format!(
        r#"globalThis.__tembraOrb={{name:"extension",apiVersion:1,
        create(self){{return {{counts:self.resource("counts",{{open(){{return {{audit:0,choices:0}};}}}})}};}},
        services:{{reporter:{{create(self){{return self.orb;}},
            methods:{{read(root){{const {{audit,choices}}=root.counts.value;return {{audit,choices}};}}}}}}}},
        contributes:{{
            "{GUARD}":{{priority:0,handle(root,context,input){{context.check();return {{value:input.value+2,blocked:true}};}}}},
            "{AUDIT}":{{priority:0,handle(root,context,input){{context.check();root.counts.value.audit+=input;}}}},
            "{CHOICE}":{{priority:0,handle(root,context,input){{context.check();root.counts.value.choices+=1;
                return input===2?84:input===1?11:undefined;}}}}
        }}}};"#
    );
    engine
        .load(
            &contract("extension")
                .source_artifact(&source, "extension", 1)
                .unwrap(),
            requires(),
        )
        .unwrap()
}

/// The real Rust contributor, packaged with priority -7 for each callback.
fn compiled_contributor(engine: &Engine, component: &[u8]) -> Orb {
    let roles = BTreeMap::from([GUARD, AUDIT, CHOICE].map(|hook| (hook.to_owned(), -7)));
    engine
        .load(
            &contract("extension")
                .component_artifact(component, "compiled", 1, &roles)
                .unwrap(),
            requires(),
        )
        .unwrap()
}

/// Typed native hooks carrying the prepared WIT signatures of the provider world.
#[derive(Clone)]
struct Hooks {
    guard: Hook<Admission, Admission>,
    audit: Hook<i32, ()>,
    choice: Hook<i32, Option<i32>>,
}

fn hooks() -> Hooks {
    let prepared = contract("provider");
    let signature = |hook| prepared.function_signature(hook, "apply", true).unwrap();
    Hooks {
        guard: Hook::waterfall_until(GUARD, |input: &Admission| input.blocked)
            .signature(signature(GUARD)),
        audit: Hook::series(AUDIT).signature(signature(AUDIT)),
        choice: Hook::bail(CHOICE).signature(signature(CHOICE)),
    }
}

/// A native participant after both guests: it records series inputs and decides any undecided
/// bail with a non-null value.
fn native_participant(calls: &Calls) -> Orb {
    let Hooks { audit, choice, .. } = hooks();
    let (audited, chosen) = (calls.clone(), calls.clone());
    Orb::new("native", 1)
        .requires(Requirement {
            signatures: Default::default(),
            hook_signatures: Default::default(),
            orb: "tools".into(),
            api_version: 1,
            methods: Default::default(),
            hooks: [AUDIT.into(), CHOICE.into()].into(),
        })
        .contribute("tools", &audit, 10, move |_, input: i32| {
            audited.borrow_mut().push(("audit", input));
            async { Ok(()) }
        })
        .contribute("tools", &choice, 10, move |_, input: i32| {
            chosen.borrow_mut().push(("choice", input));
            async { Ok(Some(99)) }
        })
}

/// A typed native provider; its guard stops once a participant blocks the admission.
fn native_provider() -> Orb {
    let declared = hooks();
    let bound = declared.clone();
    let service = contract("provider")
        .plan("tools", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .into_iter()
        .find(|service| service.name == RUNNER)
        .unwrap();
    Orb::new("tools", 1)
        .hook(declared.guard)
        .hook(declared.audit)
        .hook(declared.choice)
        .service(
            Service::new(RUNNER, move |declaration| {
                Ok((
                    declaration.hook(&bound.guard)?,
                    declaration.hook(&bound.audit)?,
                    declaration.hook(&bound.choice)?,
                ))
            })
            .signature(service.signature)
            .method("run", |hooks, context, (input,): (Admission,)| async move {
                hooks.0.run(&context, input).await
            })
            .method("audit", |hooks, context, (input,): (i32,)| async move {
                hooks.1.run(&context, input).await
            })
            .method("choose", |hooks, context, (input,): (i32,)| async move {
                hooks.2.run(&context, input).await
            }),
        )
}

/// A source provider declaring the same three compositions.
fn source_provider(engine: &Engine) -> Orb {
    let source = r#"globalThis.__tembraOrb={name:"tools",apiVersion:1,
        hooks:{guard:{composition:"waterfall"},audit:{composition:"series"},choice:{composition:"bail"}},
        services:{runner:{
            create(self){return {guard:self.hook("guard"),audit:self.hook("audit"),choice:self.hook("choice")};},
            methods:{
                run(hooks,context,input){return hooks.guard.run(context,input);},
                audit(hooks,context,input){return hooks.audit.run(context,input);},
                choose(hooks,context,input){return hooks.choice.run(context,input);}
            }}}};"#;
    engine
        .load(
            &contract("provider")
                .source_artifact(source, "tools", 1)
                .unwrap(),
            Install::default(),
        )
        .unwrap()
}

fn run(engine: Engine, component: &[u8], native: bool) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let calls = Calls::default();
    let provider = if native {
        native_provider()
    } else {
        source_provider(&engine)
    };
    host.install(vec![
        provider,
        source_extension(&engine),
        compiled_contributor(&engine, component),
        native_participant(&calls),
    ])
    .unwrap();
    let references = [RUNNER, SOURCE_REPORTER, COMPILED_REPORTER].map(|service| {
        let reference = Rc::new(host.get(service, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        (service, reference)
    });
    let call = |pool: &mut LocalPool, service: &str, method: &'static str, input: Value| {
        let called = references
            .iter()
            .find(|(name, _)| *name == service)
            .unwrap()
            .1
            .clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                called
                    .call_portable::<_, serde_json::Value>(&context, method, input)
                    .await
            }
            .boxed_local()
        }))
        .unwrap()
    };
    // Waterfall: compiled adds 20, then the source participant adds 2 and blocks.
    assert_eq!(
        call(
            &mut pool,
            RUNNER,
            "run",
            json!([{"value": 3, "blocked": false}])
        ),
        json!({"value": 25, "blocked": true})
    );
    // Series: every participant observes each input; the WIT void result is null.
    for input in [5, 7] {
        assert_eq!(
            call(&mut pool, RUNNER, "audit", json!([input])),
            Value::Null
        );
    }
    // Bail: undecided reaches native, compiled `some` stops, compiled `none` continues.
    let choices = [0, 1, 2].map(|input| call(&mut pool, RUNNER, "choose", json!([input])));
    assert_eq!(choices, [json!(99), json!(42), json!(84)]);
    assert_eq!(
        call(&mut pool, SOURCE_REPORTER, "read", json!([])),
        json!({"audit": 12, "choices": 2}),
        "the source participant saw inputs 0 and 2 only"
    );
    assert_eq!(
        call(&mut pool, COMPILED_REPORTER, "read", json!([])),
        json!({"audit": 12, "choices": 3}),
        "the compiled participant ran first for every choice"
    );
    assert_eq!(
        *calls.borrow(),
        [("audit", 5), ("audit", 7), ("choice", 0)],
        "the native fallback decided only the input both guests left undecided"
    );
    for (_, reference) in &references {
        reference.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_CONTRIBUTOR"]
fn compiled_source_and_native_participants_compose_series_bail_and_waterfall_hooks() {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let component = std::fs::read(
        std::env::var_os("TEMBRA_WASM_CONTRIBUTOR").expect("TEMBRA_WASM_CONTRIBUTOR"),
    )
    .unwrap();
    for native in [true, false] {
        for engine in [
            Engine::new(Limits::default()).unwrap(),
            Engine::interpreted(Limits::default()).unwrap(),
        ] {
            run(
                engine.interpreter(&interpreter).unwrap(),
                &component,
                native,
            );
        }
    }
}
