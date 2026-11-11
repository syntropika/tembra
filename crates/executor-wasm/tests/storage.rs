//! Standard WASI filesystem enforcement through separately configured Orb grants.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{path::PathBuf, rc::Rc};
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, FilesystemGrant, Grants, Install, Limits};

// Standard WASI through the raw generated module: read bytes through a stream, write bytes
// through `writeViaStream` with a plain async byte iterable and await its result future.
const SOURCE: &str = r#"const preopens = __tembraStandard['wasi:filesystem/preopens@0.3.0'];
const mount = (path) => { const [[root]] = preopens.getDirectories(); if (!path.startsWith('/data/')) throw new Error('outside the granted mount'); return [root, path.slice(6)]; };
const readText = async (path) => { const [root, relative] = mount(path); const fd = await root.openAt({}, relative, {}, {read: true}); const [stream, done] = fd.readViaStream(0n); let text = ''; for await (const byte of stream) text += String.fromCharCode(byte); await done; return text; };
globalThis.__tembraOrb = {services:{files:{methods:{
    read: async (_e,_c,path) => readText(path),
    write: async (_e,_c,path,content) => {
        const [root, relative] = mount(path);
        const fd = await root.openAt({}, relative, {create: true, truncate: true}, {write: true});
        const bytes = (async function* () { for (let i = 0; i < content.length; i++) yield content.charCodeAt(i) & 0xff; })();
        const result = await fd.writeViaStream(bytes, 0n);
        if (result && result.tag === 'err') throw new Error(`write failed: ${result.val}`);
    },
}}}};"#;

/// Check actual preopens, read-only access and traversal refusal in the same engine binary.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_SCRATCH"]
fn wasi_filesystem_grants_are_per_orb() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let root = PathBuf::from(std::env::var_os("TEMBRA_WASM_SCRATCH").expect("TEMBRA_WASM_SCRATCH"))
        .join(format!("filesystem-{}", std::process::id()));
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "initial").unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .interpreter(&component)
        .unwrap()
        .filesystem("data", &root)
        .unwrap();
    let contract = Contract::from_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit/storage"),
        Some("storage"),
    )
    .unwrap();
    assert!(
        engine
            .load(
                &contract.source_artifact(SOURCE, "denied", 1).unwrap(),
                Install::default()
            )
            .is_err()
    );
    let writable = Grants {
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable: true,
        }],
        ..Grants::default()
    };
    let readonly = Grants {
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable: false,
        }],
        ..Grants::default()
    };
    let writer = engine
        .load(&contract.source_artifact(SOURCE, "writer", 1).unwrap(), {
            let mut install = Install::default();
            install.grants = writable;
            install
        })
        .unwrap();
    let reader = engine
        .load(&contract.source_artifact(SOURCE, "reader", 1).unwrap(), {
            let mut install = Install::default();
            install.grants = readonly;
            install
        })
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![writer, reader]).unwrap();
    let writer = Rc::new(
        host.get(
            "writer::example:storage/files@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    let reader = Rc::new(
        host.get(
            "reader::example:storage/files@1.0.0",
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
            assert_eq!(
                writer
                    .call::<_, Value>(&context, "read", json!(["/data/input.txt"]))
                    .await?,
                "initial"
            );
            writer
                .call::<_, Value>(&context, "write", json!(["/data/input.txt", "changed"]))
                .await?;
            assert_eq!(
                reader
                    .call::<_, Value>(&context, "read", json!(["/data/input.txt"]))
                    .await?,
                "changed"
            );
            assert!(
                reader
                    .call::<_, Value>(&context, "write", json!(["/data/input.txt", "forbidden"]))
                    .await
                    .is_err()
            );
            assert!(
                writer
                    .call::<_, Value>(&context, "read", json!(["/data/../outside.txt"]))
                    .await
                    .is_err()
            );
            assert!(
                writer
                    .call::<_, Value>(&context, "read", json!(["/etc/passwd"]))
                    .await
                    .is_err()
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    writer.release().unwrap();
    reader.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert_eq!(
        std::fs::read_to_string(root.join("input.txt")).unwrap(),
        "changed"
    );
    std::fs::remove_file(root.join("input.txt")).unwrap();
    std::fs::remove_dir(root).unwrap();
}
