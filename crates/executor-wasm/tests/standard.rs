//! Exercise generated source WASI bindings under actual kernel admission and cancellation.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::{Host, Reference};
use tembra_executor_wasm::{
    Contract, Engine, FilesystemGrant, Grants, Install, Limits, SystemCapability,
};

const WIT: &str = r#"package test:standard;
interface checks { open: async func() -> string; size: async func() -> string; hold: async func() -> string; abandon: async func() -> string; burst: async func() -> string; }
world app {
 import wasi:filesystem/types@0.3.0;
 import wasi:filesystem/preopens@0.3.0;
 import wasi:clocks/monotonic-clock@0.3.0;
 export checks;
}"#;
// The raw standard-module lookup tests the interpreter boundary independently of author tooling.
const SOURCE: &str = r#"
const filesystem = __tembraStandard['wasi:filesystem/preopens@0.3.0'];
const clock = __tembraStandard['wasi:clocks/monotonic-clock@0.3.0'];
let retained;
globalThis.__tembraOrb = {services:{checks:{methods:{
 async open() { const [[root]] = filesystem.getDirectories(); retained = await root.openAt({}, 'bytes', {}, {read:true}); return (await retained.stat()).size.toString(); },
 async size() { return (await retained.stat()).size.toString(); },
 async hold() { console.log('standard:waiting'); await clock.waitFor(60000000000n); return 'finished'; },
 // Return while filesystem calls are still admitted; the timer settles first.
 async abandon() { const [[root]] = filesystem.getDirectories(); const tick = clock.waitFor(0n); for (let i = 0; i < 127; i++) root.stat().catch(() => {}); await tick; return 'abandoned'; },
 async burst() { const [[root]] = filesystem.getDirectories(); return (await Promise.all(Array.from({length: 96}, () => root.stat()))).length.toString(); }
}}}};
"#;

/// Call through an admitted kernel root; no test bypasses the native invocation owner.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: Rc<Reference>,
    method: &'static str,
) -> tembra::Result<Value> {
    pool.run_until(host.root("test", move |context| {
        async move {
            reference
                .call::<_, Value>(&context, method, json!([]))
                .await
        }
        .boxed_local()
    }))
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn source_standard_cancellation_preserves_owned_descriptors_and_retires_the_store() {
    source_standard(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn interpreted_source_standard_cancellation_preserves_owned_descriptors_and_retires_the_store() {
    source_standard(true);
}

/// The same admitted operation and cancellation run under native code and portable Pulley.
fn source_standard(interpreted: bool) {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("bytes"), [0, 255, 128, 1]).unwrap();
    let (logs, observed) = async_channel::unbounded();
    let limits = Limits {
        // Debug Pulley initialization is slower; cancellation still has its own two-second gate.
        timeout_ms: if interpreted { 30_000 } else { 3000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .interpreter(&interpreter)
    .unwrap()
    .filesystem("data", directory.path())
    .unwrap()
    .on_log(move |record| {
        let _ = logs.try_send(record.message.clone());
    });
    let contract = Contract::from_source("standard.wit", WIT, Some("app")).unwrap();
    let artifact = contract.source_artifact(SOURCE, "standard", 1).unwrap();
    assert!(engine.load(&artifact, Install::default()).is_err());
    let mut install = Install::default();
    install.grants = Grants {
        system: [SystemCapability::Clocks].into(),
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable: false,
        }],
        ..Grants::default()
    };
    let orb = engine.load(&artifact, install).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(
        host.get(
            "standard::test:standard/checks",
            vec!["main".into()],
            "test",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(
        call(&mut pool, &host, reference.clone(), "open").unwrap(),
        "4"
    );
    let held = reference.clone();
    let started = Instant::now();
    let outcome = pool.run_until(host.root("test", move |context| {
        async move {
            let cancellation = context.clone();
            let (outcome, ()) = futures::join!(
                held.call::<_, Value>(&context, "hold", json!([])),
                async move {
                    loop {
                        if observed.recv().await.unwrap().contains("standard:waiting") {
                            break;
                        }
                    }
                    cancellation.cancel().unwrap();
                }
            );
            outcome
        }
        .boxed_local()
    }));
    assert!(outcome.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation waited for the callback wall timeout"
    );
    assert_eq!(
        call(&mut pool, &host, reference.clone(), "size").unwrap(),
        "4"
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Load one granted source Orb named `name` and return its admitted service reference.
fn orb(engine: &Engine, host: &Host, pool: &mut LocalPool, name: &str) -> Rc<Reference> {
    let contract = Contract::from_source("standard.wit", WIT, Some("app")).unwrap();
    let artifact = contract.source_artifact(SOURCE, name, 1).unwrap();
    let mut install = Install::default();
    install.grants = Grants {
        system: [SystemCapability::Clocks].into(),
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable: false,
        }],
        ..Grants::default()
    };
    host.install(vec![engine.load(&artifact, install).unwrap()])
        .unwrap();
    let reference = Rc::new(
        host.get(
            &format!("{name}::test:standard/checks"),
            vec!["main".into()],
            "test",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Process-wide filesystem admission is shared by every Orb: a callback that returns with
/// admitted calls outstanding must give the slots back before its Orb runs again.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn abandoned_standard_calls_release_process_admission_while_their_orb_is_idle() {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let engine = Engine::new(Limits {
        timeout_ms: 5000,
        ..Limits::default()
    })
    .unwrap()
    .interpreter(&interpreter)
    .unwrap()
    .filesystem("data", directory.path())
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let first = orb(&engine, &host, &mut pool, "first");
    let second = orb(&engine, &host, &mut pool, "second");
    for _ in 0..3 {
        assert_eq!(
            call(&mut pool, &host, first.clone(), "abandon").unwrap(),
            "abandoned"
        );
        // The first Orb stays idle; the second needs most of the process admission now.
        assert_eq!(
            call(&mut pool, &host, second.clone(), "burst").unwrap(),
            "96"
        );
    }
    assert_eq!(
        call(&mut pool, &host, first.clone(), "burst").unwrap(),
        "96"
    );
    first.release().unwrap();
    second.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// `wasi:cli/exit` in a source Orb retires only that revision with its exact status.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE built with the v6 guest"]
fn source_exit_fails_only_its_revision_with_the_exact_status() {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let wit = r#"package test:quit;
interface run { go: async func() -> string; }
world app { import wasi:cli/exit@0.3.0; export run; }"#;
    let source = r#"
const exit = __tembraStandard['wasi:cli/exit@0.3.0'];
globalThis.__tembraOrb = {services:{run:{methods:{
 async go() {
  // Exit is not an exception: guest catch blocks cannot intercept or survive it.
  try { exit.exitWithCode(4); } catch (error) { return 'caught: ' + error; }
  return 'unreachable';
 }
}}}};
"#;
    let engine = Engine::new(Limits {
        timeout_ms: 3000,
        ..Limits::default()
    })
    .unwrap()
    .interpreter(&interpreter)
    .unwrap();
    let contract = Contract::from_source("quit.wit", wit, Some("app")).unwrap();
    let artifact = contract.source_artifact(source, "quit", 1).unwrap();
    assert!(engine.load(&artifact, Install::default()).is_err());
    let mut install = Install::default();
    install.grants = Grants {
        system: [SystemCapability::Cli].into(),
        ..Grants::default()
    };
    let orb = engine.load(&artifact, install).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(
        host.get("quit::test:quit/run", vec!["main".into()], "test")
            .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let error = call(&mut pool, &host, reference.clone(), "go")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("orb exited with status 4; revision retired"),
        "{error}"
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Install one source Orb and return its admitted `checks` reference.
fn source_orb(
    engine: &Engine,
    host: &Host,
    pool: &mut LocalPool,
    wit: &str,
    source: &str,
    name: &str,
    grants: Grants,
) -> Rc<Reference> {
    let contract = Contract::from_source("builtins.wit", wit, Some("app")).unwrap();
    let artifact = contract.source_artifact(source, name, 1).unwrap();
    let mut install = Install::default();
    install.grants = grants;
    host.install(vec![engine.load(&artifact, install).unwrap()])
        .unwrap();
    let reference = Rc::new(
        host.get(
            &format!("{name}::test:builtins/checks"),
            vec!["main".into()],
            "test",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    reference
}

const BUILTINS: &str = r#"
const attempt = (run) => { try { return String(run()); } catch (error) { return 'denied: ' + error.message; } };
globalThis.__tembraOrb = {services:{checks:{methods:{
 async probe() {
  return JSON.stringify({
   call: attempt(() => Date()),
   construct: attempt(() => new Date().getTime()),
   now: attempt(() => Date.now()),
   constructor: attempt(() => new Date(0).constructor()),
   random: attempt(() => Math.random()),
   pure: new Date(Date.UTC(2020, 1, 29)).getUTCDate() + '/' + Date.parse('1970-01-02T00:00:00Z'),
  });
 },
 async seed() {
  const module = __tembraStandard['wasi:random/insecure-seed@0.3.0'];
  return module ? module.getInsecureSeed().map(String).join(',') : 'none';
 }
}}}};
"#;

/// The first value of xorshift128+ (`state0 = left`, `state1 = right`, shifts 23/17/26), computed
/// independently of the guest for the canonical built-in seed
/// `[left, right] = [getInsecureSeed()[0], getInsecureSeed()[1]] = [low, high]`.
fn first_random(left: u64, right: u64) -> f64 {
    let (mut s1, s0) = (left, if left == 0 && right == 0 { 1 } else { right });
    s1 ^= s1 << 23;
    let state1 = s1 ^ s0 ^ (s1 >> 17) ^ (s0 >> 26);
    // The top 53 bits of `state1 + s0`, scaled to [0, 1).
    (state1.wrapping_add(s0) >> 11) as f64 / (1u64 << 53) as f64
}

/// Source built-ins follow the system grants and the engine's configured (stateful) sources.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE built with the v6 guest"]
fn source_builtins_follow_grants_and_configured_sources() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    struct Wall(u64);
    impl tembra_executor_wasm::HostWallClock for Wall {
        fn resolution(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn now(&self) -> Duration {
            Duration::from_millis(1_700_000_000_000 + self.0)
        }
    }
    let (walls, seeds) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (wall_count, seed_count) = (walls.clone(), seeds.clone());
    let engine = Engine::new(Limits {
        timeout_ms: 3000,
        ..Limits::default()
    })
    .unwrap()
    .interpreter(&interpreter)
    .unwrap()
    .wall_clock(move || Wall(wall_count.fetch_add(1, Ordering::SeqCst) + 1))
    .insecure_random_seed(move || {
        let n = seed_count.fetch_add(1, Ordering::SeqCst) + 1;
        (u128::from(0x1234_5678_u64 + n) << 64) | u128::from(0x9abc_def0_u64 * n)
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let probe = |pool: &mut LocalPool, reference: Rc<Reference>, method: &'static str| -> String {
        call(pool, &host, reference, method)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };

    // No grants: every clock and entropy path is refused and no configured source is touched.
    let plain = "package test:builtins; interface checks { probe: async func() -> string; seed: async func() -> string; } world app { export checks; }";
    let denied = source_orb(
        &engine,
        &host,
        &mut pool,
        plain,
        BUILTINS,
        "denied",
        Grants::default(),
    );
    let result: Value = serde_json::from_str(&probe(&mut pool, denied.clone(), "probe")).unwrap();
    for path in ["call", "construct", "now", "constructor"] {
        assert_eq!(
            result[path], "denied: clock capability is not granted",
            "{path}"
        );
    }
    assert_eq!(result["random"], "denied: random capability is not granted");
    assert_eq!(result["pure"], "29/86400000");
    assert_eq!(walls.load(Ordering::SeqCst), 0);
    assert_eq!(seeds.load(Ordering::SeqCst), 0);

    // Clocks and Random granted (built-ins need no standard import; the seed binding does).
    let seeded = "package test:builtins; interface checks { probe: async func() -> string; seed: async func() -> string; } world app { import wasi:random/insecure-seed@0.3.0; export checks; }";
    let grants = Grants {
        system: [SystemCapability::Clocks, SystemCapability::Random].into(),
        ..Grants::default()
    };
    let granted = source_orb(
        &engine, &host, &mut pool, seeded, BUILTINS, "granted", grants,
    );
    assert_eq!(
        walls.load(Ordering::SeqCst),
        1,
        "one clock instance for the new Store"
    );
    assert_eq!(
        seeds.load(Ordering::SeqCst),
        1,
        "one seed for the new Store"
    );
    let result: Value = serde_json::from_str(&probe(&mut pool, granted.clone(), "probe")).unwrap();
    assert_eq!(
        result["now"], "1700000000001",
        "the configured clock, not ambient time"
    );
    assert_eq!(result["construct"], "1700000000001");
    assert!(
        result["call"].as_str().unwrap().contains("2023"),
        "{}",
        result["call"]
    );
    let (high, low) = (0x1234_5678_u64 + 1, 0x9abc_def0_u64);
    let random: f64 = result["random"].as_str().unwrap().parse().unwrap();
    // The PRNG starts from the same observable tuple the browser uses: `[low, high]`.
    assert_eq!(random, first_random(low, high));
    assert_ne!(
        random,
        first_random(high, low),
        "the word order is observable"
    );
    // The standard insecure seed is the same per-Store value (low, high order per WIT).
    assert_eq!(
        probe(&mut pool, granted.clone(), "seed"),
        format!("{low},{high}")
    );
    denied.release().unwrap();
    granted.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
