//! Replace isolated heaps while the existing kernel retains old roots and compatible state.
mod support;
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

/// Run real TypeScript revisions, an aborted opener and per-realm resource cleanup.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_ORB"]
fn replace_and_abort_real_interpreted_orbs() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let bytes =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ORB").expect("TEMBRA_WASM_ORB")).unwrap();
    let prepared = tembra_contracts::Artifact::decode(&bytes).unwrap();
    let tembra_contracts::ArtifactImplementation::Source(source) = &prepared.implementation else {
        panic!("expected source");
    };
    let source = source.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .interpreter(&component)
        .unwrap();
    let contract = prepared.contract.clone();
    let install =
        || Install::default().requires("example:integrated/arithmetic@1.0.0", "native.math", 1);
    let prepared = engine
        .load(
            &contract
                .artifact(Some(&source), None, "calculator", 1)
                .unwrap(),
            install(),
        )
        .unwrap();
    let native = support::native_math(&contract, "native.math");
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![native, prepared]).unwrap();
    let reference = Rc::new(
        host.get(
            "calculator::example:integrated/calculation@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "sum", json!([{"values":[1,2,3]}]))
                    .await?,
                12
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    let (started, start) = oneshot::channel();
    let (resume, wait) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let old_host = host.clone();
    let old_reference = reference.clone();
    pool.spawner()
        .spawn_local(async move {
            let result = old_host
                .root("alice", move |context| {
                    async move {
                        assert_eq!(
                            old_reference
                                .call::<_, Value>(&context, "recover", json!([]))
                                .await?,
                            42
                        );
                        let _ = started.send(());
                        wait.await.unwrap();
                        old_reference
                            .call::<_, Value>(&context, "recover", json!([]))
                            .await
                    }
                    .boxed_local()
                })
                .await;
            let _ = finished.send(result);
        })
        .unwrap();
    pool.run_until(start).unwrap();
    let changed = source.replace("return 42;", "return 84;");
    assert_ne!(source, changed);
    let replacement = engine
        .load(
            &contract
                .artifact(Some(&changed), None, "calculator", 1)
                .unwrap(),
            install(),
        )
        .unwrap();
    pool.run_until(host.reload(vec![replacement], 5000))
        .unwrap();
    assert_eq!(engine.instances(), 2);
    let called = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "recover", json!([]))
                    .await?,
                84
            );
            assert_eq!(
                called.call::<_, Value>(&context, "read", json!([])).await?,
                12
            );
            assert_eq!(
                called
                    .call::<_, Value>(&context, "resource-value", json!([]))
                    .await?,
                42
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(pool.run_until(finish).unwrap().unwrap(), 42);
    pool.run_until(host.settled());
    assert_eq!(engine.instances(), 1);

    let validating = format!(
        r#"{changed}
const originalCreate = globalThis.__tembraOrb.services.calculation.create;
globalThis.__tembraOrb.services.calculation.create = self => originalCreate(new Proxy(self, {{get(target,key) {{
  if (key === "state") return (initial,options) => target.state(initial,{{...options,validate:value => value < 20}});
  if (key === "resource") return (name,options) => target.resource(name,{{...options,async open(signal) {{await tembra.wait(50); return options.open(signal);}}}});
  return Reflect.get(target,key);
}}}}));
"#
    );
    let candidate = engine
        .load(
            &contract
                .artifact(Some(&validating), None, "calculator", 1)
                .unwrap(),
            install(),
        )
        .unwrap();
    let changing_host = host.clone();
    let changing_reference = reference.clone();
    let (changed_state, state_changed) = oneshot::channel();
    pool.spawner()
        .spawn_local(async move {
            futures_timer::Delay::new(std::time::Duration::from_millis(20)).await;
            let result = changing_host
                .root("alice", move |context| {
                    async move {
                        changing_reference
                            .call::<_, Value>(&context, "sum", json!([{"values":[6,6]}]))
                            .await
                    }
                    .boxed_local()
                })
                .await;
            let _ = changed_state.send(result);
        })
        .unwrap();
    assert!(pool.run_until(host.reload(vec![candidate], 5000)).is_err());
    assert_eq!(pool.run_until(state_changed).unwrap().unwrap(), 24);
    assert_eq!(engine.instances(), 1);

    let failing = format!(
        "{changed}\nglobalThis.__tembraOrb.services.calculation.create = (self) => {{ const total=self.state(0); const resource=self.resource('number',{{open(){{throw new Error('opener refused')}}}}); return {{total,resource}}; }};"
    );
    let candidate = engine
        .load(
            &contract
                .artifact(Some(&failing), None, "calculator", 1)
                .unwrap(),
            install(),
        )
        .unwrap();
    assert!(pool.run_until(host.reload(vec![candidate], 5000)).is_err());
    assert_eq!(engine.instances(), 1);
    let called = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "recover", json!([]))
                    .await?,
                84
            );
            assert_eq!(
                called.call::<_, Value>(&context, "read", json!([])).await?,
                24
            );
            assert_eq!(
                called
                    .call::<_, Value>(&context, "resource-value", json!([]))
                    .await?,
                42
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
