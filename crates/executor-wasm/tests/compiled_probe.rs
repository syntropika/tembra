//! Compiled-component settlement, quarantine and standard WASI grant conformance with a real probe.
//!
//! The deadline test needs no probe: its component is encoded from a small core module.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tembra::{Host, Orb, Reference};
use tembra_executor_wasm::{Contract, Engine, FilesystemGrant, Grants, Install, Limits};

const SERVICE: &str = "example:probe/probe@1.0.0";

/// Package the probe component under one Orb name with its own grants.
fn probe(engine: &Engine, name: &str, grants: Grants) -> tembra::Result<Orb> {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_PROBE").expect("TEMBRA_WASM_PROBE")).unwrap();
    let contract = Contract::from_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit/probe"),
        Some("probe-provider"),
    )
    .unwrap();
    let mut install = Install::default();
    install.grants = grants;
    engine.load(
        &contract
            .component_artifact(&component, name, 1, &Default::default())
            .unwrap(),
        install,
    )
}

/// Mount the data provider at `/data` with the requested access.
fn data(writable: bool) -> Grants {
    Grants {
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable,
        }],
        ..Grants::default()
    }
}

/// Acquire and initialize one probe reference.
fn reference(pool: &mut LocalPool, host: &Host, name: &str) -> Rc<Reference> {
    let reference = Rc::new(
        host.get(&format!("{name}::{SERVICE}"), vec!["main".into()], "alice")
            .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Mirror the guest checksum.
fn checksum(rounds: u32) -> u32 {
    (0..rounds).fold(0u32, |checksum, round| {
        checksum.wrapping_mul(31).wrapping_add(round)
    })
}

/// Read the WIT `result` tag returned by a filesystem probe.
fn tag(value: &Value) -> &str {
    value["tag"].as_str().expect("tagged WIT result")
}

/// Trap, fuel and timeout failures quarantine only their own Orb and retire only its Store.
#[test]
#[ignore = "requires TEMBRA_WASM_PROBE"]
fn compiled_failures_quarantine_only_their_orb() {
    let scratch = tempfile::tempdir().unwrap();
    let engine = Engine::new(Limits {
        fuel: 20_000_000,
        timeout_ms: 2_000,
        ..Limits::default()
    })
    .unwrap()
    .filesystem("data", scratch.path())
    .unwrap();
    let slow = Engine::new(Limits {
        fuel: 1 << 40,
        timeout_ms: 200,
        ..Limits::default()
    })
    .unwrap()
    .filesystem("data", scratch.path())
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        probe(&engine, "trapped", data(false)).unwrap(),
        probe(&engine, "exhausted", data(false)).unwrap(),
        probe(&engine, "healthy", data(false)).unwrap(),
        probe(&slow, "busy", data(false)).unwrap(),
    ])
    .unwrap();
    let references =
        ["trapped", "exhausted", "healthy", "busy"].map(|name| reference(&mut pool, &host, name));
    assert_eq!(engine.instances(), 3);
    assert_eq!(slow.instances(), 1);
    let [trapped, exhausted, healthy, busy] = references.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                healthy
                    .call::<_, Value>(&context, "spin", json!([1000]))
                    .await?,
                checksum(1000)
            );
            let error = trapped
                .call::<_, Value>(&context, "trap", json!([]))
                .await
                .unwrap_err();
            assert!(error.message.contains("component trap"), "{error}");
            let error = trapped
                .call::<_, Value>(&context, "spin", json!([1]))
                .await
                .unwrap_err();
            assert!(error.message.contains("component trap"), "{error}");
            let error = exhausted
                .call::<_, Value>(&context, "spin", json!([u32::MAX]))
                .await
                .unwrap_err();
            assert!(error.message.contains("fuel"), "{error}");
            assert!(
                exhausted
                    .call::<_, Value>(&context, "spin", json!([1]))
                    .await
                    .is_err()
            );
            let error = busy
                .call::<_, Value>(&context, "spin", json!([u32::MAX]))
                .await
                .unwrap_err();
            assert!(error.message.contains("timed out"), "{error}");
            assert!(
                busy.call::<_, Value>(&context, "spin", json!([1]))
                    .await
                    .is_err()
            );
            // The shared compiled code keeps serving the Orb whose own Store never failed.
            assert_eq!(
                healthy
                    .call::<_, Value>(&context, "spin", json!([10]))
                    .await?,
                checksum(10)
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    // Each failed Store was dropped before its failure was published.
    assert_eq!(engine.instances(), 1);
    assert_eq!(slow.instances(), 0);
    for reference in references {
        reference.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// A timed-out call is published only after the runtime has joined its blocked WASI operation.
#[cfg(unix)]
#[test]
#[ignore = "requires TEMBRA_WASM_PROBE"]
fn compiled_timeout_waits_for_blocking_wasi_work() {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let scratch = tempfile::tempdir().unwrap();
    let fifo = scratch.path().join("pipe");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let engine = Engine::new(Limits {
        timeout_ms: 300,
        ..Limits::default()
    })
    .unwrap()
    .filesystem("data", scratch.path())
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![probe(&engine, "blocked", data(false)).unwrap()])
        .unwrap();
    let blocked = reference(&mut pool, &host, "blocked");
    let released = Arc::new(Mutex::new(None::<Instant>));
    let release = {
        let released = released.clone();
        let fifo = fifo.clone();
        std::thread::spawn(move || {
            // Keep the guest's FIFO open blocked well past the call deadline.
            std::thread::sleep(Duration::from_millis(1_000));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                *released.lock().unwrap() = Some(Instant::now());
                // A nonblocking writer succeeds only while the guest's reader is inside open.
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                {
                    Ok(_) => return true,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return false,
                }
            }
        })
    };
    let started = Instant::now();
    let called = blocked.clone();
    let observed = released.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let error = called
                .call::<_, Value>(&context, "open", json!(["/data/pipe"]))
                .await
                .unwrap_err();
            let settled = Instant::now();
            assert!(error.message.contains("timed out"), "{error}");
            let released = *observed.lock().unwrap();
            let release = released.expect("the call settled while its WASI open was still blocked");
            assert!(settled >= release);
            assert!(
                called
                    .call::<_, Value>(&context, "spin", json!([1]))
                    .await
                    .is_err()
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1_000));
    assert!(
        release.join().unwrap(),
        "the guest never blocked inside the FIFO open"
    );
    assert_eq!(engine.instances(), 0);
    blocked.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[path = "support/components.rs"]
mod components;

/// One export that logs `late`, then answers at once.
const LATE_WIT: &str = r#"package example:late@1.0.0;
interface answers { answer: func() -> u32; }
world late { import tembra:guest/context@1.0.0; export answers; }"#;

/// Level 2 is `info`; the message lies at offset 16.
const LATE_MODULE: &str = r#"(module
  (import "tembra:guest/context@1.0.0" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "late")
  (func (export "example:late/answers@1.0.0#answer") (result i32)
    i32.const 2 i32.const 16 i32.const 4 call $log
    i32.const 7))"#;

/// A call that answers only after its deadline fails with its timeout and quarantines its Orb,
/// even when its Store's thread runs the timer late. The host's log sink holds that thread past
/// the deadline, as a busy machine can. The guest answers as soon as the log returns, before
/// the timer has run.
#[test]
fn an_answer_after_the_deadline_is_a_timeout() {
    let engine = Engine::new(Limits {
        timeout_ms: 500,
        ..Limits::default()
    })
    .unwrap()
    .on_log(|record| {
        if record.message == "late" {
            std::thread::sleep(Duration::from_millis(1_000));
        }
    });
    let artifact = components::encode(LATE_WIT, "late", LATE_MODULE, "late", &Default::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![engine.load(&artifact, Install::default()).unwrap()])
        .unwrap();
    let late = Rc::new(
        host.get(
            "late::example:late/answers@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(late.ready()).unwrap();
    let answer = |pool: &mut LocalPool| {
        let called = late.clone();
        pool.run_until(host.root("alice", move |context| {
            async move { called.call::<_, Value>(&context, "answer", json!([])).await }
                .boxed_local()
        }))
    };
    let error = answer(&mut pool).unwrap_err();
    assert!(error.message.contains("timed out"), "{error}");
    let refused = answer(&mut pool).unwrap_err();
    assert_eq!(refused.reason, "wasm_executor_quarantined", "{refused}");
    late.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// A component that passes an infinite float to its import, which the portable profile cannot
/// carry, and the native provider of that import.
const INFINITE_WIT: &str = r#"package example:infinite@1.0.0;
interface sink { put: func(value: f64) -> u32; }
interface answers { answer: func() -> u32; }
world provider { export sink; }
world consumer { import sink; export answers; }"#;

const INFINITE_MODULE: &str = r#"(module
  (import "example:infinite/sink@1.0.0" "put" (func $put (param f64) (result i32)))
  (memory (export "memory") 1)
  (func (export "example:infinite/answers@1.0.0#answer") (result i32)
    f64.const inf call $put))"#;

/// A float the portable profile cannot carry in an import's argument is the executor's refusal,
/// never a result's: the call fails as the component's trap, and nothing names `invalid/result`,
/// which only a result answers.
#[test]
fn an_infinite_import_argument_is_not_a_result_refusal() {
    let sink = "example:infinite/sink@1.0.0";
    let contract = Contract::from_source("infinite.wit", INFINITE_WIT, Some("provider")).unwrap();
    let signature = contract
        .plan("sink", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0)
        .signature;
    let provider = Orb::new("sink", 1).service(
        tembra::Service::new(format!("sink::{sink}"), |_| Ok(()))
            .signature(signature)
            .method("put", |_, _, _: (f64,)| async { Ok(0_u32) }),
    );
    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = components::encode(
        INFINITE_WIT,
        "consumer",
        INFINITE_MODULE,
        "infinite",
        &Default::default(),
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        provider,
        engine
            .load(&artifact, Install::default().requires(sink, "sink", 1))
            .unwrap(),
    ])
    .unwrap();
    let answers = Rc::new(
        host.get(
            "infinite::example:infinite/answers@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(answers.ready()).unwrap();
    let called = answers.clone();
    let error = pool
        .run_until(host.root("alice", move |context| {
            async move { called.call::<_, Value>(&context, "answer", json!([])).await }
                .boxed_local()
        }))
        .unwrap_err();
    assert_eq!(error.code, tembra::Code::Unavailable, "{error}");
    assert!(!error.to_string().contains("invalid/result"), "{error}");
    answers.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// A component whose `ratio` divides its arguments, so `ratio(1, 0)` returns an infinite float,
/// and whose other functions read a parameter of each kind the lowering checks by name or length.
const RATIO_WIT: &str = r#"package example:ratio@1.0.0;
interface checks {
  enum color { red, green }
  record point { x: s32, y: s32 }
  flags mode { on, dim }
  ratio: func(a: f64, b: f64) -> f64;
  named: func(c: color) -> u32;
  shifted: func(p: point) -> s32;
  flipped: func(m: mode) -> u32;
  code: func(c: char) -> u32;
}
world provider { export checks; }"#;

const RATIO_MODULE: &str = r#"(module
  (func (export "example:ratio/checks@1.0.0#ratio") (param f64 f64) (result f64)
    local.get 0 local.get 1 f64.div)
  (func (export "example:ratio/checks@1.0.0#named") (param i32) (result i32) local.get 0)
  (func (export "example:ratio/checks@1.0.0#shifted") (param i32 i32) (result i32)
    local.get 0 local.get 1 i32.add)
  (func (export "example:ratio/checks@1.0.0#flipped") (param i32) (result i32) local.get 0)
  (func (export "example:ratio/checks@1.0.0#code") (param i32) (result i32) local.get 0))"#;

/// A compiled Orb served through a platform port answers the codes a native host answers: a result
/// the portable profile cannot carry is `invalid/result`, and arguments that do not match the
/// function's parameters are `invalid/arguments`, an unknown enum name, a record or flags with
/// another field and a string of two characters for a `char` included. Neither retires the Store,
/// and a fault of the packet stays the executor's.
#[test]
fn a_platform_port_answers_the_native_codes() {
    use tembra_executor_wasm::PortEvent;

    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = tembra_contracts::Artifact::decode(&components::encode(
        RATIO_WIT,
        "provider",
        RATIO_MODULE,
        "ratio",
        &Default::default(),
    ))
    .unwrap();
    let port = engine
        .open_port(
            &artifact,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
    port.declare(
        json!({"kind":"create", "environment":1, "service":"checks"}),
        |_, _| panic!("the provider declares no imports"),
    )
    .unwrap();
    let slot = |name: &str| {
        artifact
            .contract
            .bind("example:ratio/checks@1.0.0", name, true)
            .unwrap()
    };
    let invoke = |environment: u32, function: &str, args: Value| {
        let packet = json!({"kind":"invoke", "environment":environment, "function":slot(function), "args":args});
        let (ticket, events) = port.begin(packet.to_string()).unwrap();
        let PortEvent::Complete(result) = events.receive().unwrap() else {
            panic!("the provider calls no imports")
        };
        ticket.finish().unwrap();
        result
    };
    let call = |function: &str, args: Value| invoke(1, function, args);
    for (function, args, reason) in [
        ("ratio", json!([1, 0]), "result"),
        ("ratio", json!([0, 0]), "result"),
        ("ratio", json!(["one", 1]), "arguments"),
        ("ratio", json!([1]), "arguments"),
        ("ratio", json!([1, 2, 3]), "arguments"),
        ("ratio", json!(null), "arguments"),
        ("named", json!(["purple"]), "arguments"),
        ("shifted", json!([{ "x": 1, "y": 2, "z": 3 }]), "arguments"),
        ("shifted", json!([[1, 2]]), "arguments"),
        (
            "flipped",
            json!([{ "on": true, "bright": true }]),
            "arguments",
        ),
        ("flipped", json!([{ "on": 1 }]), "arguments"),
        ("flipped", json!([true]), "arguments"),
        ("code", json!(["ab"]), "arguments"),
        ("code", json!([""]), "arguments"),
    ] {
        let error = call(function, args.clone()).unwrap_err();
        assert_eq!(
            (error.code, error.reason.as_str()),
            (tembra::Code::Invalid, reason),
            "{function}{args}: {error}"
        );
    }
    // An environment the packet names but nobody declared is the executor's fault.
    let error = invoke(9, "ratio", json!([1, 4])).unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (tembra::Code::Unavailable, "wasm_executor"),
        "{error}"
    );
    for (function, args, value) in [
        ("ratio", json!([1, 4]), json!(0.25)),
        ("named", json!(["green"]), json!(1)),
        ("shifted", json!([{ "x": 1, "y": 2 }]), json!(3)),
        ("flipped", json!([{ "on": true, "dim": false }]), json!(1)),
        ("flipped", json!([{}]), json!(0)),
        ("code", json!(["\u{e9}"]), json!(233)),
    ] {
        let answer = call(function, args.clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&answer).unwrap(),
            value,
            "{function}{args}"
        );
    }
}

/// A monotonic clock whose reading holds the Store's thread for a second, as a busy machine can.
struct Holding;

impl tembra_executor_wasm::HostMonotonicClock for Holding {
    fn resolution(&self) -> u64 {
        1
    }

    fn now(&self) -> u64 {
        std::thread::sleep(Duration::from_millis(1_000));
        0
    }
}

/// An engine with a 500 ms deadline whose monotonic clock holds the Store's thread past it, and
/// the installation that grants the clock.
fn holding() -> (Engine, Install) {
    let engine = Engine::new(Limits {
        timeout_ms: 500,
        ..Limits::default()
    })
    .unwrap()
    .monotonic_clock(|| Holding);
    let mut install = Install::default();
    install.grants = Grants {
        system: [tembra_executor_wasm::SystemCapability::Clocks].into(),
        ..Grants::default()
    };
    (engine, install)
}

/// Package a core module of `wit`'s world `late`, which may import the WASI monotonic clock.
fn clocked(wit: &str, module: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str(
            "clocks.wit",
            include_str!("../../contracts/wit/wasi03/clocks.wit"),
        )
        .unwrap();
    let package = resolve.push_str("late.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some("late")).unwrap();
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
    Contract::from_source("late.wit", wit, Some("late"))
        .unwrap()
        .component_artifact(&component, "late", 1, &Default::default())
        .unwrap()
}

/// The answers of `LATE_WIT`, from a module that reads the clock while it initializes.
const STARTED_WIT: &str = r#"package example:late@1.0.0;
interface answers { answer: func() -> u32; }
world late { import wasi:clocks/monotonic-clock@0.3.0; export answers; }"#;

const STARTED_MODULE: &str = r#"(module
  (import "wasi:clocks/monotonic-clock@0.3.0" "now" (func $now (result i64)))
  (memory (export "memory") 1)
  (func (export "_initialize") call $now drop)
  (func (export "example:late/answers@1.0.0#answer") (result i32)
    i32.const 7))"#;

/// A component whose instantiation ends after its deadline fails with its timeout, even when
/// its Store's thread runs the timer late: the clock holds that thread past the deadline while
/// the module initializes, and instantiation ends as soon as the clock answers.
#[test]
fn an_instantiation_after_the_deadline_is_a_timeout() {
    let (engine, install) = holding();
    let error = engine
        .load(&clocked(STARTED_WIT, STARTED_MODULE), install)
        .map(drop)
        .unwrap_err();
    assert!(error.message.contains("timed out"), "{error}");
    assert_eq!(engine.instances(), 0);
}

/// One resource, whose destructor reads the clock.
const DESTROYED_WIT: &str = r#"package example:late@1.0.0;
interface things { resource thing { constructor(); } }
world late { import wasi:clocks/monotonic-clock@0.3.0; export things; }"#;

const DESTROYED_MODULE: &str = r#"(module
  (import "wasi:clocks/monotonic-clock@0.3.0" "now" (func $now (result i64)))
  (import "[export]example:late/things@1.0.0" "[resource-new]thing"
    (func $new (param i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "example:late/things@1.0.0#[constructor]thing") (result i32)
    i32.const 1 call $new)
  (func (export "example:late/things@1.0.0#[dtor]thing") (param i32)
    call $now drop))"#;

/// A destructor that ends after its deadline fails with its timeout and quarantines its Orb,
/// even when its Store's thread runs the timer late: the clock holds that thread past the
/// deadline, and the destructor returns as soon as the clock answers.
#[test]
fn a_destructor_after_the_deadline_is_a_timeout() {
    let (engine, install) = holding();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&clocked(DESTROYED_WIT, DESTROYED_MODULE), install)
            .unwrap(),
    ])
    .unwrap();
    let things = Rc::new(
        host.get(
            "late::example:late/things@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(things.ready()).unwrap();
    let construct = |pool: &mut LocalPool| {
        let called = things.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                called
                    .call_portable::<_, tembra::portable::Owned>(
                        &context,
                        "[constructor]thing",
                        json!([]),
                    )
                    .await
            }
            .boxed_local()
        }))
    };
    drop(construct(&mut pool).unwrap());
    let refused = construct(&mut pool)
        .err()
        .expect("the destructor's timeout quarantines the Orb");
    assert_eq!(refused.reason, "wasm_executor_quarantined", "{refused}");
    assert!(refused.message.contains("timed out"), "{refused}");
    things.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Compiled grants are the standard WASI preopen permissions, not the source Orb text API.
#[test]
#[ignore = "requires TEMBRA_WASM_PROBE"]
fn compiled_filesystem_grants_are_standard_and_scoped() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "initial").unwrap();
    std::fs::write(scratch.path().join("outside.txt"), "outside").unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .filesystem("data", &root)
        .unwrap();
    let denied = probe(&engine, "denied", Grants::default())
        .err()
        .expect("an ungranted filesystem import is refused");
    assert_eq!(denied.code, tembra::Code::Forbidden, "{denied}");
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        probe(&engine, "writer", data(true)).unwrap(),
        probe(&engine, "reader", data(false)).unwrap(),
    ])
    .unwrap();
    let writer = reference(&mut pool, &host, "writer");
    let reader = reference(&mut pool, &host, "reader");
    let references = (writer.clone(), reader.clone(), root.clone());
    pool.run_until(host.root("alice", move |context| {
        async move {
            let (writer, reader, root) = references;
            let call = |reference: &Rc<Reference>, method: &'static str, path: &'static str| {
                let reference = reference.clone();
                let context = context.clone();
                async move {
                    reference
                        .call::<_, Value>(&context, method, json!([path]))
                        .await
                }
            };
            assert_eq!(tag(&call(&reader, "open", "/data/input.txt").await?), "ok");
            // A read-only grant refuses every mutation inside its mount.
            assert_eq!(
                tag(&call(&reader, "create-directory", "/data/made").await?),
                "err"
            );
            assert_eq!(
                tag(&call(&reader, "create-file", "/data/blob").await?),
                "err"
            );
            assert!(!root.join("made").exists() && !root.join("blob").exists());
            // A writable grant permits standard directory and file operations, not only text.
            assert_eq!(
                tag(&call(&writer, "create-directory", "/data/made").await?),
                "ok"
            );
            assert!(root.join("made").is_dir());
            assert_eq!(
                tag(&call(&writer, "create-file", "/data/made/blob").await?),
                "ok"
            );
            assert_eq!(std::fs::metadata(root.join("made/blob")).unwrap().len(), 0);
            assert_eq!(
                tag(&call(&reader, "remove-directory", "/data/made").await?),
                "err"
            );
            assert_eq!(
                tag(&call(&writer, "create-directory", "/data/empty").await?),
                "ok"
            );
            assert_eq!(
                tag(&call(&writer, "remove-directory", "/data/empty").await?),
                "ok"
            );
            assert!(!root.join("empty").exists());
            // Both Orbs stay inside their own preopen.
            assert_eq!(
                tag(&call(&writer, "open", "/data/../outside.txt").await?),
                "err"
            );
            assert_eq!(
                tag(&call(&writer, "open", "/other/input.txt").await?),
                "err"
            );
            assert_eq!(
                tag(&call(&writer, "create-file", "/data/../escaped").await?),
                "err"
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert!(!scratch.path().join("escaped").exists());
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("outside.txt")).unwrap(),
        "outside"
    );
    writer.release().unwrap();
    reader.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Cancel queued writes before execution while a different root owns the physical component call.
#[cfg(unix)]
#[test]
#[ignore = "requires TEMBRA_WASM_PROBE"]
fn cancelled_queued_component_call_never_writes() {
    use futures::{
        channel::oneshot,
        future::{Either, select},
        task::LocalSpawnExt,
    };
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let scratch = tempfile::tempdir().unwrap();
    let fifo = scratch.path().join("pipe");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and stays alive throughout mkfifo.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let engine = Engine::new(Limits {
        timeout_ms: 15_000,
        ..Limits::default()
    })
    .unwrap()
    .filesystem("data", scratch.path())
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![probe(&engine, "queued", data(true)).unwrap()])
        .unwrap();
    let reference = reference(&mut pool, &host, "queued");
    let spawn = |principal, method: &'static str, path: &'static str| {
        let (issued, context) = oneshot::channel();
        let (finished, result) = oneshot::channel();
        let host = host.clone();
        let reference = reference.clone();
        pool.spawner()
            .spawn_local(async move {
                let value = host
                    .root(principal, move |context| {
                        async move {
                            let _ = issued.send(context.clone());
                            reference
                                .call::<_, Value>(&context, method, json!([path]))
                                .await
                        }
                        .boxed_local()
                    })
                    .await;
                let _ = finished.send(value);
            })
            .unwrap();
        (context, result)
    };
    let (context_a, finish_a) = spawn("alice", "open", "/data/pipe");
    let (context_b, finish_b) = spawn("bob", "create-file", "/data/must-not-exist");
    pool.run_until(context_a).unwrap();
    let context_b = pool.run_until(context_b).unwrap();
    pool.run_until(futures_timer::Delay::new(Duration::from_millis(20)));
    context_b.cancel().unwrap();
    let (early, pending) = pool.run_until(async {
        let deadline = std::pin::pin!(futures_timer::Delay::new(Duration::from_secs(1)));
        match select(finish_b, deadline).await {
            Either::Left((result, _)) => (Some(result.unwrap()), None),
            Either::Right((_, pending)) => (None, Some(pending)),
        }
    });
    // Release the real WASI operation even on a regression, then inspect the cancellation result.
    let writer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("compiled FIFO reader never started: {error}"),
            }
        }
    });
    assert_eq!(tag(&pool.run_until(finish_a).unwrap().unwrap()), "ok");
    if let Some(pending) = pending {
        let _ = pool.run_until(pending).unwrap();
    }
    writer.join().unwrap();
    let live = reference.clone();
    pool.run_until(host.root("carol", move |context| {
        async move {
            assert_eq!(
                live.call::<_, Value>(&context, "spin", json!([10])).await?,
                checksum(10)
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(
        !scratch.path().join("must-not-exist").exists(),
        "a cancelled queued call wrote a file"
    );
    let error = early
        .expect("queued cancellation waited for the physical call ahead of it")
        .unwrap_err();
    assert_eq!(error.code, tembra::Code::Cancelled);
}
