//! Qualify the complete generated filesystem binding against real upstream directories with raw
//! v5 packets. Every Store turn runs under `futures::executor::block_on` outside any Tokio
//! runtime: an upstream `spawn_blocking` branch would panic, so passing proves the inline
//! `allow_blocking_current_thread` branches (descriptor methods, read/write/append streams and
//! the directory iterator) that production runs inside the shared active filesystem budget.
use super::*;
use std::path::Path;
use streams::INPUT_BYTES;
use wasmtime_wasi::filesystem::FsPerms;

const CREATE: u32 = 1;
const TRUNCATE: u32 = 1 << 3;
const READ: u32 = 1;
const WRITE: u32 = 1 << 1;
const FOLLOW: u32 = 1;
const ACCESS: u32 = 0;
const NOT_PERMITTED: u32 = 30;
const READ_ONLY: u32 = 32;

/// One raw request in the guest's binary layout.
struct Packet(Vec<u8>);

impl Packet {
    fn op(operation: u8) -> Self {
        Self(vec![PACKET_TAG, operation])
    }
    fn call(name: &str) -> Self {
        let index = METHOD_NAMES
            .iter()
            .position(|known| *known == name)
            .unwrap();
        Self::op(CALL).u32(index as u32)
    }
    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn text(self, value: &str) -> Self {
        self.bytes(value.as_bytes())
    }
    fn bytes(mut self, value: &[u8]) -> Self {
        self.0
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.0.extend_from_slice(value);
        self
    }
}

type Store = wasmtime::Store<State>;

/// A Store with a writable `/rw` and a read-only `/ro` preopen, all interfaces imported.
fn store(rw: &Path, ro: &Path) -> (Store, u64, u64) {
    let engine = crate::Engine::new(crate::Limits::default()).unwrap();
    let mut builder = wasmtime_wasi::WasiCtx::builder();
    builder
        .allow_blocking_current_thread(true)
        .preopened_dir(rw, "/rw", FsPerms::ReadWrite)
        .unwrap()
        .preopened_dir(ro, "/ro", FsPerms::ReadOnly)
        .unwrap();
    let mut state = super::super::runtime::test_state(builder.build());
    for interface in INTERFACES {
        state.standard.allowed.insert((*interface).into());
    }
    let mut store = wasmtime::Store::new(&engine.engine, state);
    let reply = sync(&mut store, &Packet::call("get-directories").0).unwrap();
    let mut reader = Reader(&reply);
    assert_eq!(reader.u32().unwrap(), 2);
    let mut roots = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let id = reader.u64().unwrap();
        roots.insert(reader.string().unwrap(), id);
    }
    (store, roots["/rw"], roots["/ro"])
}

fn sync(store: &mut Store, packet: &[u8]) -> Result<Vec<u8>, String> {
    synchronous_now(store.as_context_mut(), packet).unwrap()
}

/// Run one asynchronous packet to completion in its own Store turn.
fn run(store: &mut Store, packet: &[u8]) -> Result<Vec<u8>, String> {
    futures::executor::block_on(store.run_concurrent(async |access| {
        let admitted = access.with(|mut store| spawn(store.as_context_mut(), packet));
        match admitted.unwrap() {
            Ok((_task, _leases, receive)) => receive.await.unwrap().map(|done| done.bytes),
            Err(error) => Err(error),
        }
    }))
    .unwrap()
}

/// Let other Store tasks run inside `run_concurrent` without a Tokio runtime.
async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if std::mem::replace(&mut yielded, true) {
            std::task::Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

fn open(store: &mut Store, dir: u64, path: &str, open: u32, flags: u32) -> Result<u64, u32> {
    let reply = run(
        store,
        &Packet::call("[method]descriptor.open-at")
            .u64(dir)
            .u32(FOLLOW)
            .text(path)
            .u32(open)
            .u32(flags)
            .0,
    )
    .unwrap();
    let mut reader = Reader(&reply);
    match reader.u8().unwrap() {
        0 => Ok(reader.u64().unwrap()),
        _ => Err(reader.u32().unwrap()),
    }
}

/// `result<_, error-code>` replies: `Ok` or the error-code case.
fn unit(reply: Vec<u8>) -> Result<(), u32> {
    let mut reader = Reader(&reply);
    match reader.u8().unwrap() {
        0 => Ok(()),
        _ => Err(reader.u32().unwrap()),
    }
}

/// Call a descriptor method whose remaining parameters are paths and borrows.
fn path_call(store: &mut Store, name: &str, packet: Packet) -> Result<(), u32> {
    let _ = name;
    unit(run(store, &packet.0).unwrap())
}

fn size(store: &mut Store, fd: u64) -> u64 {
    let reply = run(store, &Packet::call("[method]descriptor.stat").u64(fd).0).unwrap();
    let mut reader = Reader(&reply);
    assert_eq!(reader.u8().unwrap(), 0);
    assert_ne!(reader.u32().unwrap(), 7);
    reader.u64().unwrap();
    reader.u64().unwrap()
}

fn stream_new(store: &mut Store) -> u64 {
    let reply = sync(store, &Packet::op(STREAM_NEW).u32(BYTE_STREAM).0).unwrap();
    u64::from_le_bytes(reply.try_into().unwrap())
}

fn write(store: &mut Store, stream: u64, chunk: &[u8]) -> Result<Vec<u8>, String> {
    run(store, &Packet::op(STREAM_WRITE).u64(stream).bytes(chunk).0)
}

fn release(store: &mut Store, id: u64) -> Result<Vec<u8>, String> {
    sync(store, &Packet::op(DROP).u64(id).0)
}

fn future(store: &mut Store, id: u64) -> Result<(), u32> {
    unit(run(store, &Packet::op(FUTURE_READ).u64(id).0).unwrap())
}

/// Read a byte stream to its end.
fn read_all(store: &mut Store, stream: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let reply = run(store, &Packet::op(STREAM_READ).u64(stream).u32(1 << 16).0).unwrap();
        assert!(reply.len() <= store.data().packet_bytes);
        let mut reader = Reader(&reply);
        if reader.u8().unwrap() == 1 {
            return bytes;
        }
        bytes.extend_from_slice(reader.bytes().unwrap());
    }
}

/// Write `data` through a new input stream with `call` = write-via-stream or append-via-stream.
fn stream_into(store: &mut Store, call: Packet, data: &[u8]) -> Result<(), u32> {
    let input = stream_new(store);
    let future_id = u64::from_le_bytes(
        sync(store, &call.0.replace_stream(input))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    for chunk in data.chunks(40_000) {
        // Upstream stops reading after an error: the write reports the closed reader, an
        // ordinary status, and the future then reports that error.
        match write(store, input, chunk).unwrap().as_slice() {
            [WRITE_ACCEPTED] => {}
            [WRITE_CLOSED] => break,
            other => panic!("unexpected write status {other:?}"),
        }
    }
    release(store, input).unwrap();
    future(store, future_id)
}

/// Packets for stream calls are built with a placeholder stream id replaced here.
trait ReplaceStream {
    fn replace_stream(self, input: u64) -> Vec<u8>;
}

const PLACEHOLDER: u64 = 0xdead_beef_dead_beef;

impl ReplaceStream for Vec<u8> {
    fn replace_stream(mut self, input: u64) -> Vec<u8> {
        let needle = PLACEHOLDER.to_le_bytes();
        let at = self.windows(8).position(|window| window == needle).unwrap();
        self[at..at + 8].copy_from_slice(&input.to_le_bytes());
        self
    }
}

#[test]
fn byte_streams_write_append_truncate_and_read_non_utf8_at_exact_offsets() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, root, _) = store(rw.path(), ro.path());
    let fd = open(
        &mut store,
        root,
        "data.bin",
        CREATE | TRUNCATE,
        READ | WRITE,
    )
    .unwrap();
    let data: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let call = Packet::call("[method]descriptor.write-via-stream")
        .u64(fd)
        .u64(PLACEHOLDER)
        .u64(3);
    stream_into(&mut store, call, &data).unwrap();
    let call = Packet::call("[method]descriptor.append-via-stream")
        .u64(fd)
        .u64(PLACEHOLDER);
    stream_into(&mut store, call, &[0xaa, 0xff]).unwrap();
    let mut expected = vec![0, 0, 0];
    expected.extend_from_slice(&data);
    expected.extend_from_slice(&[0xaa, 0xff]);
    assert_eq!(std::fs::read(rw.path().join("data.bin")).unwrap(), expected);
    let reply = sync(
        &mut store,
        &Packet::call("[method]descriptor.read-via-stream")
            .u64(fd)
            .u64(0)
            .0,
    )
    .unwrap();
    let (stream, done) = (
        u64::from_le_bytes(reply[..8].try_into().unwrap()),
        u64::from_le_bytes(reply[8..].try_into().unwrap()),
    );
    assert_eq!(read_all(&mut store, stream), expected);
    assert_eq!(future(&mut store, done), Ok(()));
    release(&mut store, stream).unwrap();
    // An offset that overflows the file position reports a WIT error, not a trap.
    let call = Packet::call("[method]descriptor.write-via-stream")
        .u64(fd)
        .u64(PLACEHOLDER)
        .u64(u64::MAX - 1);
    let input = stream_new(&mut store);
    let done = u64::from_le_bytes(
        sync(&mut store, &call.0.replace_stream(input))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let _ = write(&mut store, input, &[1, 2, 3, 4]);
    release(&mut store, input).unwrap();
    assert!(future(&mut store, done).is_err());
    assert_eq!(std::fs::read(rw.path().join("data.bin")).unwrap(), expected);
    let truncate = Packet::call("[method]descriptor.set-size").u64(fd).u64(2);
    assert_eq!(unit(run(&mut store, &truncate.0).unwrap()), Ok(()));
    assert_eq!(size(&mut store, fd), 2);
    release(&mut store, fd).unwrap();
    release(&mut store, root).unwrap();
    store.assert_concurrent_state_empty();
}

#[test]
fn input_streams_apply_backpressure_and_serialize_writes() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, root, _) = store(rw.path(), ro.path());
    let fd = open(&mut store, root, "pressure", CREATE, READ | WRITE).unwrap();
    let input = stream_new(&mut store);
    write(&mut store, input, &vec![1; PIPE_BYTES]).unwrap();
    let call = Packet::call("[method]descriptor.write-via-stream")
        .u64(fd)
        .u64(input)
        .u64(0);
    futures::executor::block_on(store.run_concurrent(async |access| {
        // Nothing consumes the full buffer yet: the next write waits, a third is refused.
        let (_task, _leases, waiting) = access
            .with(|mut store| {
                spawn(
                    store.as_context_mut(),
                    &Packet::op(STREAM_WRITE).u64(input).bytes(&[2]).0,
                )
            })
            .unwrap()
            .unwrap();
        let mut waiting = std::pin::pin!(waiting);
        for _ in 0..4 {
            yield_now().await;
        }
        assert!(futures::poll!(&mut waiting).is_pending());
        let refused = access
            .with(|mut store| {
                spawn(
                    store.as_context_mut(),
                    &Packet::op(STREAM_WRITE).u64(input).bytes(&[3]).0,
                )
            })
            .unwrap();
        assert!(refused.err().unwrap().contains("already pending"));
        // Transferring the readable end lets upstream drain the buffer.
        let done = access
            .with(|mut store| synchronous_now(store.as_context_mut(), &call.0))
            .unwrap()
            .unwrap();
        waiting.await.unwrap().unwrap();
        access
            .with(|mut store| {
                synchronous_now(store.as_context_mut(), &Packet::op(DROP).u64(input).0)
            })
            .unwrap()
            .unwrap();
        u64::from_le_bytes(done.try_into().unwrap())
    }))
    .map(|done| assert_eq!(future(&mut store, done), Ok(())))
    .unwrap();
    assert_eq!(
        std::fs::metadata(rw.path().join("pressure")).unwrap().len(),
        PIPE_BYTES as u64 + 1
    );
    release(&mut store, fd).unwrap();
    store.assert_concurrent_state_empty();
}

#[test]
fn input_stream_transfer_is_transactional() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, root, _) = store(rw.path(), ro.path());
    let fd = open(&mut store, root, "file", CREATE, READ | WRITE).unwrap();
    let input = stream_new(&mut store);
    let call = |fd: u64| {
        Packet::call("[method]descriptor.write-via-stream")
            .u64(fd)
            .u64(input)
            .u64(0)
    };
    let mut trailing = call(fd).0;
    trailing.push(0);
    assert!(
        sync(&mut store, &trailing)
            .unwrap_err()
            .contains("trailing")
    );
    assert!(
        sync(&mut store, &call(input).0)
            .unwrap_err()
            .contains("different type")
    );
    assert!(
        sync(&mut store, &call(u64::MAX >> 11).0)
            .unwrap_err()
            .contains("unknown")
    );
    assert!(sync(&mut store, &Packet::op(STREAM_NEW).u32(BYTE_STREAM + 1).0).is_err());
    // None of the refused calls consumed the readable end.
    let done = sync(&mut store, &call(fd).0).unwrap();
    assert!(
        sync(&mut store, &call(fd).0)
            .unwrap_err()
            .contains("already transferred")
    );
    write(&mut store, input, b"\xffok").unwrap();
    release(&mut store, input).unwrap();
    assert_eq!(
        future(&mut store, u64::from_le_bytes(done.try_into().unwrap())),
        Ok(())
    );
    assert_eq!(std::fs::read(rw.path().join("file")).unwrap(), b"\xffok");
    // A writer from another Store is not a name in this one.
    let (mut other, _, _) = self::store(rw.path(), ro.path());
    let foreign = stream_new(&mut other);
    assert!(
        sync(
            &mut store,
            &Packet::call("[method]descriptor.write-via-stream")
                .u64(fd)
                .u64(foreign)
                .u64(0)
                .0
        )
        .unwrap_err()
        .contains("unknown")
    );
    assert!(
        write(&mut store, foreign, b"x")
            .unwrap_err()
            .contains("not a live writable stream")
    );
    release(&mut store, fd).unwrap();
    assert!(
        run(
            &mut store,
            &Packet::call("[method]descriptor.stat").u64(fd).0
        )
        .unwrap_err()
        .contains("unknown")
    );
}

/// Decode `u32 count` + directory entries (`descriptor-type` case, `string name`).
fn entries(reply: &[u8], names: &mut Vec<(u32, String)>) -> usize {
    let mut reader = Reader(reply);
    assert_eq!(reader.u8().unwrap(), 0);
    let count = reader.u32().unwrap() as usize;
    for _ in 0..count {
        let kind = reader.u32().unwrap();
        assert_ne!(kind, 7);
        names.push((kind, reader.string().unwrap()));
    }
    reader.end().unwrap();
    count
}

#[test]
fn read_directory_streams_typed_entries_in_bounded_batches() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for index in 0..2000 {
        std::fs::write(
            rw.path()
                .join(format!("entry-{index:04}-{}", "x".repeat(60))),
            [0xff],
        )
        .unwrap();
    }
    std::fs::create_dir(rw.path().join("sub")).unwrap();
    let (mut store, root, _) = store(rw.path(), ro.path());
    let reply = sync(
        &mut store,
        &Packet::call("[method]descriptor.read-directory")
            .u64(root)
            .0,
    )
    .unwrap();
    let (stream, done) = (
        u64::from_le_bytes(reply[..8].try_into().unwrap()),
        u64::from_le_bytes(reply[8..].try_into().unwrap()),
    );
    // The inline directory iterator pauses once the bounded pipe is full.
    futures::executor::block_on(store.run_concurrent(async |_| {
        for _ in 0..64 {
            yield_now().await;
        }
    }))
    .unwrap();
    let Some(Entry::Stream(pipe)) = store.data().standard.handles.entries.get(&stream) else {
        panic!("typed stream")
    };
    let buffered = pipe.lock().unwrap().buffered();
    assert!(buffered > 0 && buffered <= PIPE_BYTES + 128, "{buffered}");
    let mut names = Vec::new();
    loop {
        let reply = run(&mut store, &Packet::op(STREAM_READ).u64(stream).u32(500).0).unwrap();
        assert!(reply.len() <= store.data().packet_bytes);
        if reply == [1] {
            break;
        }
        assert!(entries(&reply, &mut names) <= 500);
    }
    assert_eq!(future(&mut store, done), Ok(()));
    assert_eq!(names.len(), 2001);
    assert!(names.contains(&(2, "sub".into())));
    assert_eq!(names.iter().filter(|(kind, _)| *kind == 5).count(), 2000);
    release(&mut store, stream).unwrap();
    store.assert_concurrent_state_empty();
}

#[test]
fn permissions_traversal_and_symlink_escapes_fail_before_side_effects() {
    let (rw, ro, outside) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::write(ro.path().join("keep"), b"\x80keep").unwrap();
    std::fs::write(rw.path().join("a"), b"\xfe").unwrap();
    std::fs::write(outside.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), rw.path().join("link")).unwrap();
    let (mut store, rw_root, ro_root) = store(rw.path(), ro.path());
    // Read-only mount: no creation, no writes through a read-only descriptor.
    assert!(open(&mut store, ro_root, "new", CREATE, READ | WRITE).is_err());
    assert!(!ro.path().join("new").exists());
    let keep = open(&mut store, ro_root, "keep", 0, READ).unwrap();
    let call = Packet::call("[method]descriptor.write-via-stream")
        .u64(keep)
        .u64(PLACEHOLDER)
        .u64(0);
    assert!(stream_into(&mut store, call, b"overwrite").is_err());
    assert_eq!(std::fs::read(ro.path().join("keep")).unwrap(), b"\x80keep");
    // Traversal and symlink escapes.
    assert_eq!(
        open(&mut store, rw_root, "../escape", CREATE, READ | WRITE),
        Err(NOT_PERMITTED)
    );
    assert!(open(&mut store, rw_root, "link", 0, READ).is_err());
    let unlink = |dir, path: &str| {
        Packet::call("[method]descriptor.unlink-file-at")
            .u64(dir)
            .text(path)
    };
    assert!(path_call(&mut store, "unlink", unlink(ro_root, "keep")).is_err());
    assert!(ro.path().join("keep").exists());
    // Two-descriptor operations check both descriptors before any side effect.
    let rename = |from, old: &str, to, new: &str| {
        Packet::call("[method]descriptor.rename-at")
            .u64(from)
            .text(old)
            .u64(to)
            .text(new)
    };
    let link = |from, old: &str, to, new: &str| {
        Packet::call("[method]descriptor.link-at")
            .u64(from)
            .u32(0)
            .text(old)
            .u64(to)
            .text(new)
    };
    let refused = path_call(&mut store, "rename", rename(rw_root, "a", ro_root, "a2"));
    assert!(
        matches!(refused, Err(READ_ONLY | NOT_PERMITTED | ACCESS)),
        "{refused:?}"
    );
    assert!(path_call(&mut store, "link", link(rw_root, "a", ro_root, "a3")).is_err());
    assert!(
        rw.path().join("a").exists()
            && !ro.path().join("a2").exists()
            && !ro.path().join("a3").exists()
    );
    assert_eq!(
        path_call(&mut store, "rename", rename(rw_root, "a", rw_root, "b")),
        Ok(())
    );
    assert_eq!(
        path_call(&mut store, "link", link(rw_root, "b", rw_root, "c")),
        Ok(())
    );
    assert_eq!(std::fs::read(rw.path().join("c")).unwrap(), b"\xfe");
    assert_eq!(
        path_call(&mut store, "unlink", unlink(rw_root, "c")),
        Ok(())
    );
    let mkdir = Packet::call("[method]descriptor.create-directory-at")
        .u64(rw_root)
        .text("d");
    assert_eq!(path_call(&mut store, "mkdir", mkdir), Ok(()));
    let rmdir = Packet::call("[method]descriptor.remove-directory-at")
        .u64(rw_root)
        .text("d");
    assert_eq!(path_call(&mut store, "rmdir", rmdir), Ok(()));
    assert!(!rw.path().join("c").exists() && !rw.path().join("d").exists());
    let mut same = |a, b| {
        run(
            &mut store,
            &Packet::call("[method]descriptor.is-same-object")
                .u64(a)
                .u64(b)
                .0,
        )
        .unwrap()
    };
    assert_eq!(same(rw_root, rw_root), vec![1]);
    assert_eq!(same(rw_root, ro_root), vec![0]);
    // A writer is not a descriptor; a dropped descriptor is not a name.
    let input = stream_new(&mut store);
    assert!(
        run(
            &mut store,
            &Packet::call("[method]descriptor.stat").u64(input).0
        )
        .unwrap_err()
        .contains("different type")
    );
    release(&mut store, keep).unwrap();
    assert!(
        run(
            &mut store,
            &Packet::call("[method]descriptor.stat").u64(keep).0
        )
        .unwrap_err()
        .contains("unknown")
    );
}

#[test]
fn abandoned_writes_and_dropped_directory_streams_release_their_state() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for index in 0..1500 {
        std::fs::write(rw.path().join(format!("{index:05}-{}", "y".repeat(80))), []).unwrap();
    }
    let (mut store, root, _) = store(rw.path(), ro.path());
    let input = stream_new(&mut store);
    write(&mut store, input, &vec![0; PIPE_BYTES]).unwrap();
    let mut tasks = Tasks::default();
    let (task, leases, _) = spawn(
        store.as_context_mut(),
        &Packet::op(STREAM_WRITE).u64(input).bytes(&[1]).0,
    )
    .unwrap()
    .unwrap();
    tasks.push(task, leases);
    let reply = sync(
        &mut store,
        &Packet::call("[method]descriptor.read-directory")
            .u64(root)
            .0,
    )
    .unwrap();
    let (stream, done) = (
        u64::from_le_bytes(reply[..8].try_into().unwrap()),
        u64::from_le_bytes(reply[8..].try_into().unwrap()),
    );
    let first = run(&mut store, &Packet::op(STREAM_READ).u64(stream).u32(10).0).unwrap();
    assert_eq!(entries(&first, &mut Vec::new()), 10);
    // The consumer drops its directory stream and the callback ends with a write pending.
    release(&mut store, stream).unwrap();
    release(&mut store, done).unwrap();
    futures::executor::block_on(retire(&mut store, &mut tasks)).unwrap();
    // Retire aborted the pending write and closed the callback's untransferred writer, whose
    // accepted bytes had no reader; the iterator and its future are gone.
    assert!(
        spawn(
            store.as_context_mut(),
            &Packet::op(STREAM_WRITE).u64(input).bytes(&[]).0
        )
        .unwrap()
        .err()
        .unwrap()
        .contains("not a live writable stream")
    );
    assert!(release(&mut store, input).is_err());
    let detached = store
        .data()
        .standard
        .handles
        .entries
        .values()
        .filter(|entry| matches!(entry, Entry::Detached(_)))
        .count();
    assert_eq!(detached, 0);
    assert_eq!(
        store.data().standard.handles.input.lock().unwrap().used(),
        0
    );
    store.assert_concurrent_state_empty();
}

#[test]
fn oversized_and_malicious_stream_writes_are_refused_and_buffers_stay_bounded() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, _, _) = store(rw.path(), ro.path());
    let input = stream_new(&mut store);
    // One byte over the per-write bound is refused before buffering.
    let refused = run(
        &mut store,
        &Packet::op(STREAM_WRITE)
            .u64(input)
            .bytes(&vec![0; PIPE_BYTES + 1])
            .0,
    );
    assert!(refused.unwrap_err().contains("exceeds 65536"));
    // A length prefix larger than the packet is truncated input, not an allocation.
    let lying = Packet::op(STREAM_WRITE).u64(input).u32(u32::MAX).0;
    assert!(run(&mut store, &lying).unwrap_err().contains("truncated"));
    // Each writer holds at most one buffer; the revision holds at most INPUT_BYTES in total.
    let writers: Vec<u64> = (0..INPUT_BYTES / PIPE_BYTES)
        .map(|_| stream_new(&mut store))
        .collect();
    for writer in &writers {
        write(&mut store, *writer, &vec![7; PIPE_BYTES]).unwrap();
    }
    let Some(Entry::Writer(inbox)) = store.data().standard.handles.entries.get(&writers[0]) else {
        panic!("writer")
    };
    assert_eq!(inbox.lock().unwrap().buffered(), PIPE_BYTES);
    futures::executor::block_on(store.run_concurrent(async |access| {
        // The revision budget is exhausted: a write to a fresh writer waits.
        let (_task, _leases, waiting) = access
            .with(|mut store| {
                spawn(
                    store.as_context_mut(),
                    &Packet::op(STREAM_WRITE).u64(input).bytes(&[1]).0,
                )
            })
            .unwrap()
            .unwrap();
        let mut waiting = std::pin::pin!(waiting);
        for _ in 0..4 {
            yield_now().await;
        }
        assert!(futures::poll!(&mut waiting).is_pending());
        // Closing an untransferred writer discards its bytes and wakes the waiter.
        access
            .with(|mut store| {
                synchronous_now(store.as_context_mut(), &Packet::op(DROP).u64(writers[0]).0)
            })
            .unwrap()
            .unwrap();
        waiting.await.unwrap().unwrap();
    }))
    .unwrap();
    assert_eq!(
        store.data().standard.handles.input.lock().unwrap().used(),
        INPUT_BYTES - PIPE_BYTES + 1
    );
}

#[test]
fn accepted_input_settles_when_its_callback_ends_or_is_cancelled_without_a_guest_drop() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, root, _) = store(rw.path(), ro.path());
    let fd = open(&mut store, root, "out", CREATE, READ | WRITE).unwrap();
    let input = stream_new(&mut store);
    // 64 KiB accepted while nothing consumes, then transferred with no Store turn in between.
    write(&mut store, input, &vec![0xab; PIPE_BYTES]).unwrap();
    let call = Packet::call("[method]descriptor.write-via-stream")
        .u64(fd)
        .u64(input)
        .u64(0);
    let done = u64::from_le_bytes(sync(&mut store, &call.0).unwrap().try_into().unwrap());
    // The guest's pump is gone: a second chunk is still waiting when the callback is cancelled.
    let mut tasks = Tasks::default();
    let (task, leases, _) = spawn(
        store.as_context_mut(),
        &Packet::op(STREAM_WRITE).u64(input).bytes(&[1; 10]).0,
    )
    .unwrap()
    .unwrap();
    tasks.push(task, leases);
    let permits = store.data().standard.handles.files.available_permits();
    futures::executor::block_on(retire(&mut store, &mut tasks)).unwrap();
    // Accepted bytes reached the file in the retire turn; the unaccepted chunk never did.
    assert_eq!(
        std::fs::read(rw.path().join("out")).unwrap(),
        vec![0xab; PIPE_BYTES]
    );
    assert!(release(&mut store, input).unwrap_err().contains("unknown"));
    assert_eq!(
        store.data().standard.handles.files.available_permits(),
        permits + 1
    );
    assert_eq!(
        store.data().standard.handles.input.lock().unwrap().used(),
        0
    );
    // An explicitly retained future still reports the settled result in a later callback.
    assert_eq!(future(&mut store, done), Ok(()));
    release(&mut store, fd).unwrap();
    release(&mut store, root).unwrap();
    store.assert_concurrent_state_empty();
}

#[test]
fn the_shared_open_file_budget_counts_descriptors_and_physically_live_streams() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(rw.path().join("a"), vec![0xffu8; 1 << 20]).unwrap();
    let (mut store, root, _) = store(rw.path(), ro.path());
    // Every source Orb of an engine shares this budget; three units make exhaustion visible.
    store.data_mut().standard.handles.files = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
    let a = open(&mut store, root, "a", 0, READ).unwrap();
    let b = open(&mut store, root, "b", CREATE, READ | WRITE).unwrap();
    let reply = sync(
        &mut store,
        &Packet::call("[method]descriptor.read-via-stream")
            .u64(a)
            .u64(0)
            .0,
    )
    .unwrap();
    let (stream, done) = (
        u64::from_le_bytes(reply[..8].try_into().unwrap()),
        u64::from_le_bytes(reply[8..].try_into().unwrap()),
    );
    // Exhausted: refused at dispatch, before upstream could create the file.
    let packet = Packet::call("[method]descriptor.open-at")
        .u64(root)
        .u32(0)
        .text("c")
        .u32(CREATE)
        .u32(READ | WRITE);
    assert!(
        run(&mut store, &packet.0)
            .unwrap_err()
            .contains("open-file budget")
    );
    assert!(!rw.path().join("c").exists());
    assert!(
        sync(
            &mut store,
            &Packet::call("[method]descriptor.read-via-stream")
                .u64(b)
                .u64(0)
                .0
        )
        .unwrap_err()
        .contains("open-file budget")
    );
    // Dropping the descriptor does not close the file its stream still reads.
    release(&mut store, a).unwrap();
    let c = open(&mut store, root, "c", CREATE, READ | WRITE).unwrap();
    assert!(
        run(&mut store, &packet.0)
            .unwrap_err()
            .contains("open-file budget")
    );
    // Once the stream is dropped and upstream lets its producer go, the unit returns.
    release(&mut store, stream).unwrap();
    release(&mut store, done).unwrap();
    futures::executor::block_on(retire(&mut store, &mut Tasks::default())).unwrap();
    let d = open(&mut store, root, "d", CREATE, READ | WRITE).unwrap();
    for id in [b, c, d, root] {
        release(&mut store, id).unwrap();
    }
    assert_eq!(store.data().standard.handles.files.available_permits(), 3);
    store.assert_concurrent_state_empty();
}

/// `new-timestamp`: 0 no-change, 1 now, 2 timestamp(instant { s64 seconds, u32 nanoseconds }).
fn timestamp(packet: Packet, value: Option<(i64, u32)>) -> Packet {
    match value {
        None => packet.u32(0),
        Some((seconds, nanoseconds)) => {
            let mut packet = packet.u32(2);
            packet.0.extend_from_slice(&seconds.to_le_bytes());
            packet.u32(nanoseconds)
        }
    }
}

/// `descriptor-stat` access and modification instants from a stat/stat-at reply.
fn instants(reply: &[u8]) -> (u32, [Option<(i64, u32)>; 2]) {
    let mut reader = Reader(reply);
    assert_eq!(reader.u8().unwrap(), 0);
    let kind = reader.u32().unwrap();
    assert_ne!(kind, 7);
    reader.u64().unwrap();
    reader.u64().unwrap();
    let mut instant = || {
        (reader.u8().unwrap() == 1).then(|| {
            let seconds = i64::from_le_bytes(reader.take(8).unwrap().try_into().unwrap());
            (seconds, reader.u32().unwrap())
        })
    };
    (kind, [instant(), instant()])
}

/// A `u8 0` + u32 result such as get-flags or get-type.
fn word(reply: Vec<u8>) -> Result<u32, u32> {
    let mut reader = Reader(&reply);
    let tag = reader.u8().unwrap();
    let value = reader.u32().unwrap();
    if tag == 0 { Ok(value) } else { Err(value) }
}

#[test]
fn remaining_descriptor_operations_run_real_upstream_with_exact_values_and_errors() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(ro.path().join("keep"), b"keep").unwrap();
    std::fs::write(rw.path().join("other"), b"other").unwrap();
    let (mut store, root, ro_root) = store(rw.path(), ro.path());
    let fd = open(&mut store, root, "f", CREATE, READ | WRITE).unwrap();
    let method = |name: &str| Packet::call(&format!("[method]descriptor.{name}"));
    // advise: every advice case; an unknown case is refused at decode.
    for advice in 0..6 {
        let packet = method("advise").u64(fd).u64(0).u64(4096).u32(advice);
        assert_eq!(
            unit(run(&mut store, &packet.0).unwrap()),
            Ok(()),
            "advice {advice}"
        );
    }
    let bad = method("advise").u64(fd).u64(0).u64(1).u32(6);
    assert!(
        run(&mut store, &bad.0)
            .unwrap_err()
            .contains("unknown enum case")
    );
    for name in ["sync", "sync-data"] {
        assert_eq!(
            unit(run(&mut store, &method(name).u64(fd).0).unwrap()),
            Ok(()),
            "{name}"
        );
    }
    // get-flags reflects the open rights; get-type the object kind.
    assert_eq!(
        word(run(&mut store, &method("get-flags").u64(fd).0).unwrap()),
        Ok(READ | WRITE)
    );
    let ro_flags = word(run(&mut store, &method("get-flags").u64(ro_root).0).unwrap()).unwrap();
    assert_eq!(ro_flags & WRITE, 0);
    assert_eq!(
        word(run(&mut store, &method("get-type").u64(fd).0).unwrap()),
        Ok(5)
    );
    assert_eq!(
        word(run(&mut store, &method("get-type").u64(root).0).unwrap()),
        Ok(2)
    );
    // set-times with an exact negative (pre-epoch) instant; the other time is unchanged.
    let before = instants(&run(&mut store, &method("stat").u64(fd).0).unwrap()).1[1];
    let set = timestamp(
        timestamp(method("set-times").u64(fd), Some((-1_000_000, 123_456_789))),
        None,
    );
    assert_eq!(unit(run(&mut store, &set.0).unwrap()), Ok(()));
    let (_, [access, modified]) = instants(&run(&mut store, &method("stat").u64(fd).0).unwrap());
    assert_eq!(access, Some((-1_000_000, 123_456_789)));
    assert_eq!(modified, before);
    // set-times-at by path, then stat-at agrees exactly.
    let at = method("set-times-at").u64(root).u32(0).text("f");
    let at = timestamp(timestamp(at, None), Some((1_700_000_000, 5)));
    assert_eq!(unit(run(&mut store, &at.0).unwrap()), Ok(()));
    let stat_at = method("stat-at").u64(root).u32(0).text("f");
    assert_eq!(
        instants(&run(&mut store, &stat_at.0).unwrap()).1[1],
        Some((1_700_000_000, 5))
    );
    // Upstream normalizes an out-of-range nanosecond field into seconds (Duration::new), so a
    // 1e9 ns instant is stored as the next whole second; an unknown case is refused at decode.
    let carry = timestamp(
        timestamp(method("set-times").u64(fd), Some((0, 1_000_000_000))),
        None,
    );
    assert_eq!(unit(run(&mut store, &carry.0).unwrap()), Ok(()));
    let carried = instants(&run(&mut store, &method("stat").u64(fd).0).unwrap()).1[0];
    assert_eq!(carried, Some((1, 0)));
    let unknown = method("set-times").u64(fd).u32(3).u32(0);
    assert!(
        run(&mut store, &unknown.0)
            .unwrap_err()
            .contains("unknown variant case")
    );
    // A read-only mount refuses time changes.
    let keep = open(&mut store, ro_root, "keep", 0, READ).unwrap();
    let readonly = timestamp(timestamp(method("set-times").u64(keep), Some((1, 0))), None);
    assert!(unit(run(&mut store, &readonly.0).unwrap()).is_err());
    let readonly_at = timestamp(
        timestamp(
            method("set-times-at").u64(ro_root).u32(0).text("keep"),
            Some((1, 0)),
        ),
        None,
    );
    assert!(unit(run(&mut store, &readonly_at.0).unwrap()).is_err());
    // metadata-hash identifies objects: same through descriptor and path, different otherwise.
    let hash = run(&mut store, &method("metadata-hash").u64(fd).0).unwrap();
    assert_eq!(hash[0], 0);
    assert_eq!(
        run(
            &mut store,
            &method("metadata-hash-at").u64(root).u32(0).text("f").0
        )
        .unwrap(),
        hash
    );
    assert_ne!(
        run(
            &mut store,
            &method("metadata-hash-at").u64(root).u32(0).text("other").0
        )
        .unwrap(),
        hash
    );
    // symlink-at/readlink-at round trip; stat-at without follow sees the link itself.
    let symlink =
        |dir, target: &str, name: &str| method("symlink-at").u64(dir).text(target).text(name);
    assert_eq!(
        unit(run(&mut store, &symlink(root, "f", "lnk").0).unwrap()),
        Ok(())
    );
    let link = run(&mut store, &method("readlink-at").u64(root).text("lnk").0).unwrap();
    let mut reader = Reader(&link);
    assert_eq!(reader.u8().unwrap(), 0);
    assert_eq!(reader.string().unwrap(), "f");
    assert_eq!(
        instants(
            &run(
                &mut store,
                &method("stat-at").u64(root).u32(0).text("lnk").0
            )
            .unwrap()
        )
        .0,
        4
    );
    assert!(
        unit(run(&mut store, &method("readlink-at").u64(root).text("other").0).unwrap()).is_err()
    );
    assert!(unit(run(&mut store, &symlink(ro_root, "keep", "lnk").0).unwrap()).is_err());
    assert!(!ro.path().join("lnk").exists());
    // An absolute or escaping target never yields access outside the mount.
    for target in ["/etc/passwd", "../../outside"] {
        let _ = run(&mut store, &symlink(root, target, "escape").0).unwrap();
        assert!(
            open(&mut store, root, "escape", 0, READ).is_err(),
            "{target}"
        );
        let _ = run(
            &mut store,
            &method("unlink-file-at").u64(root).text("escape").0,
        );
    }
    for id in [keep, fd, root, ro_root] {
        release(&mut store, id).unwrap();
    }
    store.assert_concurrent_state_empty();
}

/// Payload bytes of a byte-stream chunk reply.
fn chunk_bytes(reply: &[u8]) -> Vec<u8> {
    let mut reader = Reader(reply);
    assert_eq!(reader.u8().unwrap(), 0);
    let bytes = reader.bytes().unwrap().to_vec();
    reader.end().unwrap();
    bytes
}

/// Stream and future handles of a `read-via-stream` or `read-directory` reply.
fn pair(reply: &[u8]) -> (u64, u64) {
    (
        u64::from_le_bytes(reply[..8].try_into().unwrap()),
        u64::from_le_bytes(reply[8..16].try_into().unwrap()),
    )
}

fn read_packet(stream: u64, limit: u32) -> Vec<u8> {
    Packet::op(STREAM_READ).u64(stream).u32(limit).0
}

#[test]
fn concurrent_reads_of_real_byte_and_typed_streams_are_fifo_without_loss() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let data: Vec<u8> = (0..200_000u32)
        .map(|index| (index * 7 % 251) as u8)
        .collect();
    std::fs::write(rw.path().join("a"), &data).unwrap();
    std::fs::create_dir(rw.path().join("d")).unwrap();
    for index in 0..40 {
        std::fs::write(rw.path().join(format!("d/e-{index:02}")), [1]).unwrap();
    }
    let (mut store, root, _) = store(rw.path(), ro.path());
    let fd = open(&mut store, root, "a", 0, READ).unwrap();
    let dir = open(&mut store, root, "d", 2, READ).unwrap();
    let (bytes, bytes_done) = pair(
        &sync(
            &mut store,
            &Packet::call("[method]descriptor.read-via-stream")
                .u64(fd)
                .u64(0)
                .0,
        )
        .unwrap(),
    );
    let (items, items_done) = pair(
        &sync(
            &mut store,
            &Packet::call("[method]descriptor.read-directory").u64(dir).0,
        )
        .unwrap(),
    );
    // Every read is admitted before either upstream producer first runs.
    let replies = futures::executor::block_on(store.run_concurrent(async |access| {
        let mut admitted = Vec::new();
        for _ in 0..4 {
            for packet in [read_packet(bytes, 1 << 16), read_packet(items, 7)] {
                admitted.push(
                    access
                        .with(|mut store| spawn(store.as_context_mut(), &packet))
                        .unwrap()
                        .unwrap(),
                );
            }
        }
        let mut replies = Vec::new();
        for (_task, _leases, receive) in admitted {
            replies.push(receive.await.unwrap().unwrap().bytes);
        }
        replies
    }))
    .unwrap();
    let mut read = Vec::new();
    let mut names = Vec::new();
    for pair in replies.chunks(2) {
        read.extend(chunk_bytes(&pair[0]));
        assert!(entries(&pair[1], &mut names) > 0);
    }
    // Served in admission order: concatenated chunks are an exact prefix, nothing skipped.
    assert_eq!(read, data[..read.len()]);
    read.extend(read_all(&mut store, bytes));
    assert_eq!(read, data);
    loop {
        let reply = run(&mut store, &read_packet(items, 7)).unwrap();
        if reply == [1] {
            break;
        }
        entries(&reply, &mut names);
    }
    let mut unique: Vec<_> = names.iter().map(|(_, name)| name.clone()).collect();
    unique.sort();
    unique.dedup();
    assert_eq!((names.len(), unique.len()), (40, 40));
    assert_eq!(future(&mut store, bytes_done), Ok(()));
    assert_eq!(future(&mut store, items_done), Ok(()));
    for id in [bytes, items, fd, dir, root] {
        release(&mut store, id).unwrap();
    }
    futures::executor::block_on(retire(&mut store, &mut Tasks::default())).unwrap();
    store.assert_concurrent_state_empty();
}

/// How a completed STREAM_READ reply fails to reach the guest.
#[derive(Clone, Copy, Debug)]
enum Lost {
    /// The callback finished or was cancelled in the turn the read completed (`retire` then
    /// `discard`, exactly as the step driver does).
    Discarded,
    /// The callback owner dropped the reply receiver before the read completed.
    OwnerGone,
}

#[test]
fn an_undelivered_stream_read_poisons_byte_and_typed_streams() {
    use futures::stream::FuturesUnordered;
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let data: Vec<u8> = (0..150_000u32).map(|index| (index % 253) as u8).collect();
    std::fs::write(rw.path().join("a"), &data).unwrap();
    std::fs::create_dir(rw.path().join("d")).unwrap();
    for index in 0..30 {
        std::fs::write(rw.path().join(format!("d/e-{index:02}")), [1]).unwrap();
    }
    for typed in [false, true] {
        for lost in [Lost::Discarded, Lost::OwnerGone] {
            let (mut store, root, _) = store(rw.path(), ro.path());
            let (stream, done) = if typed {
                let dir = open(&mut store, root, "d", 2, READ).unwrap();
                let reply = sync(
                    &mut store,
                    &Packet::call("[method]descriptor.read-directory").u64(dir).0,
                );
                release(&mut store, dir).unwrap();
                pair(&reply.unwrap())
            } else {
                let fd = open(&mut store, root, "a", 0, READ).unwrap();
                let reply = sync(
                    &mut store,
                    &Packet::call("[method]descriptor.read-via-stream")
                        .u64(fd)
                        .u64(0)
                        .0,
                );
                release(&mut store, fd).unwrap();
                pair(&reply.unwrap())
            };
            let packet = read_packet(stream, if typed { 5 } else { 1 << 15 });
            // A read admitted and aborted before it ran drained nothing: no poison, no loss.
            let mut tasks = Tasks::default();
            let (task, leases, _) = spawn(store.as_context_mut(), &packet).unwrap().unwrap();
            tasks.push(task, leases);
            futures::executor::block_on(retire(&mut store, &mut tasks)).unwrap();
            let first = run(&mut store, &packet).unwrap();
            if typed {
                let mut names = Vec::new();
                assert_eq!(entries(&first, &mut names), 5);
            } else {
                assert_eq!(chunk_bytes(&first), data[..1 << 15]);
            }
            // The live producer holds its file unit until it is physically stopped.
            let live = store.data().standard.handles.files.available_permits();
            // A read that completes but whose reply the guest never receives.
            let (task, leases, receive) = spawn(store.as_context_mut(), &packet).unwrap().unwrap();
            tasks.push(task, leases);
            let mut pending = FuturesUnordered::new();
            match lost {
                Lost::Discarded => {
                    pending.push(super::super::steps::standard_effect(9, Ok(receive)));
                }
                Lost::OwnerGone => drop(receive),
            }
            futures::executor::block_on(store.run_concurrent(async |_| {
                for _ in 0..8 {
                    yield_now().await;
                }
            }))
            .unwrap();
            futures::executor::block_on(retire(&mut store, &mut tasks)).unwrap();
            super::super::steps::discard(&mut store, &mut pending).unwrap();
            // Later reads (a later callback) fail instead of skipping the lost data.
            for _ in 0..2 {
                assert_eq!(
                    run(&mut store, &packet).unwrap_err(),
                    "stream read was abandoned",
                    "typed={typed} {lost:?}"
                );
            }
            // The physical producer was stopped and its file unit returned while the guest
            // still names the poisoned stream; DROP then retires it.
            futures::executor::block_on(retire(&mut store, &mut Tasks::default())).unwrap();
            if !typed {
                assert_eq!(
                    store.data().standard.handles.files.available_permits(),
                    live + 1,
                    "{lost:?}"
                );
            }
            release(&mut store, stream).unwrap();
            let _ = future(&mut store, done);
            futures::executor::block_on(retire(&mut store, &mut Tasks::default())).unwrap();
            assert!(!store.data_mut().standard.handles.purge());
            store.assert_concurrent_state_empty();
        }
    }
}

#[test]
fn one_revision_cannot_exhaust_the_open_file_pool_shared_by_engines() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let data = vec![0x5au8; 100_000];
    std::fs::write(rw.path().join("a"), &data).unwrap();
    // Two Engines, two revisions, one process-wide pool of 16 with a private share of 2 each.
    let budget = crate::engine::FileBudget::new(16);
    assert_eq!(budget.share, 2);
    let (mut hog, hog_rw, hog_ro) = store(rw.path(), ro.path());
    let (mut other, other_rw, other_ro) = store(rw.path(), ro.path());
    hog.data_mut().standard.handles.use_budget(&budget);
    other.data_mut().standard.handles.use_budget(&budget);
    let pool = || budget.process.available_permits();
    // `get-directories` in a loop: the shared preopen clones are bounded by the share.
    let roots = sync(&mut hog, &Packet::call("get-directories").0).unwrap();
    assert_eq!(pool(), 14);
    let error = sync(&mut hog, &Packet::call("get-directories").0).unwrap_err();
    assert!(
        error.contains("share of the host open-file budget"),
        "{error}"
    );
    let error = run(
        &mut hog,
        &Packet::call("[method]descriptor.open-at")
            .u64(hog_rw)
            .u32(FOLLOW)
            .text("a")
            .u32(0)
            .u32(READ)
            .0,
    )
    .unwrap_err();
    assert!(
        error.contains("share of the host open-file budget"),
        "{error}"
    );
    // Refusals hold nothing; the other revision still opens and reads from the same pool.
    assert_eq!(pool(), 14);
    let fd = open(&mut other, other_rw, "a", 0, READ).unwrap();
    let (stream, done) = pair(
        &sync(
            &mut other,
            &Packet::call("[method]descriptor.read-via-stream")
                .u64(fd)
                .u64(0)
                .0,
        )
        .unwrap(),
    );
    assert_eq!(pool(), 12);
    // The stream keeps its file unit after its descriptor drops, until it is physically done.
    release(&mut other, fd).unwrap();
    assert_eq!(pool(), 13);
    assert_eq!(read_all(&mut other, stream), data);
    assert_eq!(future(&mut other, done), Ok(()));
    release(&mut other, stream).unwrap();
    futures::executor::block_on(retire(&mut other, &mut Tasks::default())).unwrap();
    assert_eq!(pool(), 14);
    // Releasing the hog's descriptors returns both its share and the pool units.
    let mut reader = Reader(&roots);
    for _ in 0..reader.u32().unwrap() {
        release(&mut hog, reader.u64().unwrap()).unwrap();
        reader.string().unwrap();
    }
    assert_eq!(pool(), 16);
    let fd = open(&mut hog, hog_rw, "a", 0, READ).unwrap();
    for (store, ids) in [
        (&mut hog, [fd, hog_rw, hog_ro]),
        (&mut other, [other_rw, other_ro, 0]),
    ] {
        for id in ids.into_iter().filter(|id| *id != 0) {
            release(store, id).unwrap();
        }
    }
    assert_eq!(pool(), 16);
}

#[test]
fn retirement_flushes_every_accepted_input_byte_at_the_full_input_budget() {
    let (rw, ro) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut store, root, _) = store(rw.path(), ro.path());
    // Sixteen transferred writers hold the whole 1 MiB revision budget when the callback ends.
    let writers = INPUT_BYTES / PIPE_BYTES;
    let mut prepared = Vec::new();
    for index in 0..writers {
        let name = format!("out-{index:02}");
        let fd = open(&mut store, root, &name, CREATE, READ | WRITE).unwrap();
        let input = stream_new(&mut store);
        write(&mut store, input, &vec![index as u8; PIPE_BYTES]).unwrap();
        prepared.push((name, fd, input));
    }
    // All transferred with no Store turn in between, so every accepted byte is still buffered.
    let mut files = Vec::new();
    for (name, fd, input) in prepared {
        let call = Packet::call("[method]descriptor.write-via-stream")
            .u64(fd)
            .u64(input)
            .u64(0);
        let done = u64::from_le_bytes(sync(&mut store, &call.0).unwrap().try_into().unwrap());
        files.push((name, fd, done));
    }
    let used = store.data().standard.handles.input.lock().unwrap().used();
    assert_eq!(used, INPUT_BYTES);
    futures::executor::block_on(retire(&mut store, &mut Tasks::default())).unwrap();
    // Nothing accepted is left in a Feed when the Orb goes idle: disposal could not lose it.
    let used = store.data().standard.handles.input.lock().unwrap().used();
    assert_eq!(used, 0);
    assert!(store.data_mut().standard.handles.unsettled().is_none());
    for (index, (name, fd, done)) in files.into_iter().enumerate() {
        assert_eq!(
            std::fs::read(rw.path().join(name)).unwrap(),
            vec![index as u8; PIPE_BYTES]
        );
        assert_eq!(future(&mut store, done), Ok(()));
        release(&mut store, fd).unwrap();
    }
    release(&mut store, root).unwrap();
    store.assert_concurrent_state_empty();
}
