//! Compiled-component settlement, quarantine and standard WASI grant conformance with a real probe.
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
