//! Standard WASI filesystem enforcement through separately configured Orb grants.
//!
//! The writer and reader are the prepared SDK source Orbs `tests/source/test-storage-{writer,
//! reader}`: the same code under two names, installed with different grants. A refused path or
//! filesystem call is a WIT `result` error, so each refusal is a value and the revision keeps
//! serving.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use source::fixture;
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, FilesystemGrant, Grants, Install, Limits};

/// The `/data` mount, writable or read-only, and nothing else: the filesystem types only name
/// clock types, which needs no Clocks grant.
fn grants(writable: bool) -> Install {
    let mut install = Install::default();
    install.grants = Grants {
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable,
        }],
        ..Grants::default()
    };
    install
}

/// Check actual preopens, read-only access and traversal refusal in the same engine binary.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn wasi_filesystem_grants_are_per_orb() {
    for interpreted in [false, true] {
        filesystem_grants(interpreted);
    }
}

fn filesystem_grants(interpreted: bool) {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("mounted");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "initial").unwrap();
    // A real file next to the mount: traversal must not reach it.
    std::fs::write(scratch.path().join("outside.txt"), "secret").unwrap();
    let limits = Limits {
        timeout_ms: if interpreted { 30_000 } else { 5000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .filesystem("data", &root)
    .unwrap();
    // The filesystem imports need a grant: an ungranted installation is refused.
    assert!(
        engine
            .load(&fixture("test-storage-writer"), Install::default())
            .is_err()
    );
    let writer = engine
        .load(&fixture("test-storage-writer"), grants(true))
        .unwrap();
    let reader = engine
        .load(&fixture("test-storage-reader"), grants(false))
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![writer, reader]).unwrap();
    let writer = Rc::new(
        host.get(
            "writer::test:storage/files@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    let reader = Rc::new(
        host.get(
            "reader::test:storage/files@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(writer.ready()).unwrap();
    pool.run_until(reader.ready()).unwrap();
    let references = (writer.clone(), reader.clone());
    pool.run_until(host.root("alice", move |context| {
        async move {
            let (writer, reader) = references;
            let ok = |value: Value| json!({"tag": "ok", "val": value});
            assert_eq!(
                writer
                    .call::<_, Value>(&context, "read", json!(["/data/input.txt"]))
                    .await?,
                ok(json!("initial"))
            );
            assert_eq!(
                writer
                    .call::<_, Value>(&context, "write", json!(["/data/input.txt", "changed"]))
                    .await?,
                json!({"tag": "ok"}),
            );
            assert_eq!(
                reader
                    .call::<_, Value>(&context, "read", json!(["/data/input.txt"]))
                    .await?,
                ok(json!("changed"))
            );
            // Each refusal is the call's business error; the revision keeps serving.
            for (refused, method, arguments) in [
                (&reader, "write", json!(["/data/input.txt", "forbidden"])),
                (&writer, "read", json!(["/data/../outside.txt"])),
                (&writer, "read", json!(["/etc/passwd"])),
            ] {
                let result = refused
                    .call::<_, Value>(&context, method, arguments.clone())
                    .await?;
                assert_eq!(result["tag"], "err", "{method} {arguments}: {result}");
                assert!(
                    !result["val"].as_str().unwrap().is_empty(),
                    "{method} {arguments}: {result}"
                );
            }
            assert_eq!(
                writer
                    .call::<_, Value>(&context, "read", json!(["/data/input.txt"]))
                    .await?,
                ok(json!("changed"))
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    writer.release().unwrap();
    reader.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(engine.instances(), 0);
    assert_eq!(
        std::fs::read_to_string(root.join("input.txt")).unwrap(),
        "changed"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.path().join("outside.txt")).unwrap(),
        "secret"
    );
}
