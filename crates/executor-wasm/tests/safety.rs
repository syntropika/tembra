//! Qualify cleanup, scope identity, concurrent calls and termination against the real engine.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const WIT: &str = "package example:safety; interface checks { run: async func(mode: string) -> u32; } world test { export checks; }";
const SOURCE: &str = r#"
let saved;
let savedContext;
let deferredDenied = false;
Promise.resolve().then(() => {
  try { __hostSync('{"operation":"resource.declare","name":"rogue","version":1,"open":1,"close":2}'); }
  catch { deferredDenied = true; }
});
globalThis.__tembraOrb = {services:{checks:{
  create(self) {
    const state = self.state(1,{validate:value => value < 1000});
    if (!saved) saved = state;
    return {state};
  },
  methods:{async run(environment,context,mode) {
    if (mode === "save-context") { savedContext = context; return 1; }
    if (mode === "stale-context") { try { savedContext.check(); } catch { return 2; } return 0; }
    if (mode === "deferred") return deferredDenied ? 1 : 0;
    if (mode === "cached") {
      try { return saved.value; } catch { return 2; }
    }
    if (mode === "floating") {
      (async () => {
        try { await tembra.wait(3000); } catch {}
        environment.state.update(context, () => 999);
      })().catch(() => {});
      return 1;
    }
    if (mode === "many") {
      const results = await Promise.allSettled(Array.from({length:256}, () => tembra.wait(1)));
      return results.length;
    }
    if (mode === "wait") { await tembra.wait(10); return 1; }
    if (mode === "long-wait") { await tembra.wait(80); return 1; }
    if (mode === "state-checks") {
      const before = environment.state.value;
      for (const invalid of [NaN,undefined,new Date(0),1001,-0]) {
        let rejected = false;
        try { environment.state.update(context,() => invalid); } catch { rejected = true; }
        if (!rejected || environment.state.value !== before) throw new Error("Invalid state was adopted");
      }
      let computations = 0;
      const selected = environment.state.select(value => { computations++; return {doubled:value*2}; });
      const first = selected.value;
      if (first !== selected.value || !Object.isFrozen(first) || computations !== 1) throw new Error("Readable identity changed");
      environment.state.update(context,value => value+1);
      if (selected.value.doubled !== 4 || computations !== 2 || first.doubled !== 2) throw new Error("Readable snapshot changed");
      environment.state.update(context,() => before);
      return 1;
    }
    if (mode === "hang") { await new Promise(() => {}); }
    if (mode === "cpu") { while (true) {} }
    return environment.state.value;
  }}
}}};"#;

/// Run real callbacks, then prove each interpreter worker is released by native shutdown.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn scope_cleanup_and_termination() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let engine = Engine::new(Limits {
        timeout_ms: 500,
        ..Limits::default()
    })
    .unwrap()
    .interpreter(&component)
    .unwrap();
    let contract = Contract::from_source("safety.wit", WIT, None).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(
                &contract.source_artifact(SOURCE, "safe", 1).unwrap(),
                Install::default(),
            )
            .unwrap(),
    ])
    .unwrap();
    let one = Rc::new(
        host.get("safe::example:safety/checks", vec!["one".into()], "alice")
            .unwrap(),
    );
    let two = Rc::new(
        host.get("safe::example:safety/checks", vec!["two".into()], "bob")
            .unwrap(),
    );
    pool.run_until(one.ready()).unwrap();
    pool.run_until(two.ready()).unwrap();
    let calls = (one.clone(), two.clone());
    pool.run_until(host.root("alice", move |context| {
        async move {
            let (one, two) = calls;
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["save-context"]))
                    .await?,
                1
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["stale-context"]))
                    .await?,
                2
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["deferred"]))
                    .await?,
                1
            );
            assert_eq!(
                two.call::<_, Value>(&context, "run", json!(["cached"]))
                    .await?,
                2
            );
            let start = Instant::now();
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["floating"]))
                    .await?,
                1
            );
            assert!(start.elapsed() < Duration::from_millis(500));
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["read"]))
                    .await?,
                1
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["many"]))
                    .await?,
                256
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["state-checks"]))
                    .await?,
                1
            );
            let (first, second) = futures::join!(
                one.call::<_, Value>(&context, "run", json!(["wait"])),
                one.call::<_, Value>(&context, "run", json!(["wait"]))
            );
            assert_eq!(first?, 1);
            assert_eq!(second?, 1);
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["read"]))
                    .await?,
                1
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    let calls = one.clone();
    let creating_host = host.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let running = calls.call::<_, Value>(&context, "run", json!(["long-wait"]));
            let creating = async move {
                futures_timer::Delay::new(Duration::from_millis(10)).await;
                let reference = creating_host.get(
                    "safe::example:safety/checks",
                    vec!["during-call".into()],
                    "alice",
                )?;
                reference.ready().await?;
                reference.release()?;
                Ok::<(), tembra::Error>(())
            };
            let (called, created) = futures::join!(running, creating);
            assert_eq!(called?, 1);
            created?;
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    one.release().unwrap();
    two.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);

    for mode in ["hang", "cpu"] {
        let host = Host::new(pool.spawner());
        host.install(vec![
            engine
                .load(
                    &contract.source_artifact(SOURCE, "limited", 1).unwrap(),
                    Install::default(),
                )
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(
            host.get(
                "limited::example:safety/checks",
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        );
        pool.run_until(reference.ready()).unwrap();
        let called = reference.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                let error = called
                    .call::<_, Value>(&context, "run", json!([mode]))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("interpreter trap"));
                let error = called
                    .call::<_, Value>(&context, "run", json!(["read"]))
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "wasm_executor_quarantined");
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

/// Creating a new object on a causally occupied interpreter fails without quarantining that VM.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn causal_factory_wait_is_bounded_and_recoverable() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let engine = Engine::new(Limits {
        timeout_ms: 1000,
        ..Limits::default()
    })
    .unwrap()
    .interpreter(&component)
    .unwrap();
    let wit = "package example:safety; interface bridge { bounce: async func() -> u32; } interface checks { run: async func(mode:string) -> u32; } world provider {export bridge;} world test {import bridge;export checks;}";
    let contract = Contract::from_source("checks.wit", wit, Some("test")).unwrap();
    let source = format!(
        r#"{SOURCE}
const create=globalThis.__tembraOrb.services.checks.create;
const run=globalThis.__tembraOrb.services.checks.methods.run;
globalThis.__tembraOrb.services.checks.create=self => ({{...create(self),bridge:self.get("example:safety/bridge")}});
globalThis.__tembraOrb.services.checks.methods.run=(e,c,mode) => mode === "reentry" ? e.bridge.bounce(c) : run(e,c,mode);
"#
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let creating = host.clone();
    let bridge = tembra::Orb::new("bridge", 1).service(
        tembra::Service::new("bridge::example:safety/bridge", |_| Ok(()))
            .signature(
                Contract::from_bytes(&contract.encode().unwrap(), Some("provider"))
                    .unwrap()
                    .plan("bridge", 1, &Default::default())
                    .unwrap()
                    .orb
                    .services
                    .remove(0)
                    .signature,
            )
            .method("bounce", move |_, _, _: Vec<Value>| {
                let host = creating.clone();
                async move {
                    let next = host.get(
                        "safe::example:safety/checks",
                        vec!["reentrant".into()],
                        "alice",
                    )?;
                    next.ready().await?;
                    Ok(0_u32)
                }
            }),
    );
    host.install(vec![
        bridge,
        engine
            .load(
                &contract.source_artifact(&source, "safe", 1).unwrap(),
                Install::default().requires("example:safety/bridge", "bridge", 1),
            )
            .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get("safe::example:safety/checks", vec!["main".into()], "alice")
            .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let error = called
                .call::<_, Value>(&context, "run", json!(["reentry"]))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("factory admission"), "{error}");
            assert_eq!(
                called
                    .call::<_, Value>(&context, "run", json!(["read"]))
                    .await?,
                1
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
