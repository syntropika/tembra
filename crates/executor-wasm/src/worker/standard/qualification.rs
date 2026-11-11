//! Qualify binding identity, trap boundaries and synchronous Store-resource semantics with raw
//! standard packets in the same binary layout as the guest.
use super::*;
use std::{sync::atomic::AtomicU64, time::Duration};
use wasmtime::component::Resource;
use wasmtime_wasi::{
    filesystem::{FsPerms, WasiFilesystem},
    p3::bindings::filesystem::types,
};

/// Packet header: tag and operation.
fn packet(operation: u8) -> Vec<u8> {
    vec![PACKET_TAG, operation]
}

/// `CALL method handle...`; handle identities are little-endian u64.
fn call(name: &str, handles: &[u64]) -> Vec<u8> {
    let index = METHOD_NAMES
        .iter()
        .position(|known| *known == name)
        .unwrap() as u32;
    let mut bytes = packet(CALL);
    bytes.extend_from_slice(&index.to_le_bytes());
    for handle in handles {
        bytes.extend_from_slice(&handle.to_le_bytes());
    }
    bytes
}

fn release_packet(id: u64) -> Vec<u8> {
    let mut bytes = packet(DROP);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes
}

fn stream_read(id: u64, limit: u32) -> Vec<u8> {
    let mut bytes = packet(STREAM_READ);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&limit.to_le_bytes());
    bytes
}

fn future_read(id: u64) -> Vec<u8> {
    let mut bytes = packet(FUTURE_READ);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes
}

/// `open-at` arguments after the borrowed descriptor: path flags, path, open flags, flags.
fn open_at(root: u64, path: &str) -> Vec<u8> {
    let mut bytes = call("[method]descriptor.open-at", &[root]);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
    bytes.extend_from_slice(path.as_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes
}

fn sync(store: impl AsContextMut<Data = State>, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut store = store;
    synchronous_now(store.as_context_mut(), bytes).unwrap()
}

/// A Store with one read-only preopen and every Phase 1 interface imported.
fn store(directory: &std::path::Path) -> wasmtime::Store<State> {
    let engine = crate::Engine::new(crate::Limits::default()).unwrap();
    let mut builder = wasmtime_wasi::WasiCtx::builder();
    builder
        .allow_blocking_current_thread(true)
        .preopened_dir(directory, "/data", FsPerms::ReadOnly)
        .unwrap();
    let mut state = super::super::runtime::test_state(builder.build());
    for interface in INTERFACES {
        state.standard.allowed.insert((*interface).into());
    }
    wasmtime::Store::new(&engine.engine, state)
}

/// Issue the preopen through `get-directories`; returns its handle and upstream rep.
fn preopen(mut store: impl AsContextMut<Data = State>) -> Result<(u64, u32), String> {
    let reply = sync(store.as_context_mut(), &call("get-directories", &[]))?;
    let mut reader = Reader(&reply);
    assert_eq!(reader.u32().unwrap(), 1);
    let id = reader.u64().unwrap();
    assert_eq!(reader.string().unwrap(), "/data");
    reader.end().unwrap();
    match store
        .as_context_mut()
        .data()
        .standard
        .handles
        .entries
        .get(&id)
    {
        Some(Entry::Resource { rep, .. }) => Ok((id, *rep)),
        _ => panic!("issued descriptor"),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn upstream_traps_are_distinct_from_refused_packets() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    assert!(
        sync(&mut store, &release_packet(0))
            .unwrap_err()
            .contains("unknown or dropped")
    );
    // A live issued handle with a broken upstream table is an actual host trap, not WIT err.
    let id = store
        .data_mut()
        .standard
        .handles
        .own(0, Resource::<()>::new_own(u32::MAX));
    assert!(synchronous_now(store.as_context_mut(), &release_packet(id)).is_err());
}

#[test]
fn identities_are_safe_u64_values_that_never_wrap() {
    let counter = AtomicU64::new(u64::from(u32::MAX));
    assert_eq!(handles::next_identity(&counter).unwrap(), 1 << 32);
    let counter = AtomicU64::new(crate::abi::IDENTITY_LIMIT - 1);
    assert_eq!(
        handles::next_identity(&counter).unwrap(),
        crate::abi::IDENTITY_LIMIT
    );
    let exhausted = handles::next_identity(&counter).unwrap_err();
    assert!(exhausted.is::<Refused>());
    assert!(handles::next_identity(&counter).is_err());
}

#[test]
fn random_bytes_are_capped_to_one_exact_packet() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let mut request = call("get-random-bytes", &[]);
    request.extend_from_slice(&u64::MAX.to_le_bytes());
    let reply = sync(&mut store, &request).unwrap();
    assert_eq!(reply.len(), store.data().packet_bytes);
    let mut request = call("get-random-bytes", &[]);
    request.extend_from_slice(&16u64.to_le_bytes());
    assert_eq!(sync(&mut store, &request).unwrap().len(), 20);
}

#[test]
fn a_pending_borrow_refuses_drop_and_keeps_its_upstream_slot() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let (root, rep) = preopen(&mut store).unwrap();
    runtime().block_on(async {
        store
            .run_concurrent(async |access| {
                // The stat task is spawned but not yet polled, as in the guest sequence.
                let (_task, leases, receive) = access
                    .with(|mut store| {
                        spawn(
                            store.as_context_mut(),
                            &call("[method]descriptor.stat", &[root]),
                        )
                    })
                    .unwrap()
                    .unwrap();
                let refused = access.with(|mut store| sync(&mut store, &release_packet(root)));
                assert!(refused.unwrap_err().contains("borrowed by a pending call"));
                // The rep stays allocated, so upstream cannot hand it to another descriptor.
                let (other, other_rep) = access.with(|mut store| preopen(&mut store)).unwrap();
                assert_ne!(other_rep, rep);
                let delivery = receive.await.unwrap().unwrap();
                assert_eq!(delivery.bytes[0], 0, "stat ran on the lent descriptor");
                drop(leases);
                access.with(|mut store| {
                    sync(&mut store, &release_packet(root)).unwrap();
                    sync(&mut store, &release_packet(other)).unwrap();
                });
            })
            .await
            .unwrap();
        store.assert_concurrent_state_empty();
    });
}

#[test]
fn revoking_a_pending_borrow_releases_its_lend_before_upstream_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let (root, rep) = preopen(&mut store).unwrap();
    let (task, leases, receive) = spawn(
        store.as_context_mut(),
        &call("[method]descriptor.stat", &[root]),
    )
    .unwrap()
    .unwrap();
    let mut tasks = Tasks::default();
    tasks.push(task, leases);
    assert!(sync(&mut store, &release_packet(root)).is_err());
    // Callback revocation aborts the task and ends its borrow synchronously.
    drop(tasks);
    sync(&mut store, &release_packet(root)).unwrap();
    let (other, other_rep) = preopen(&mut store).unwrap();
    assert_eq!(other_rep, rep, "upstream reused the freed slot");
    runtime().block_on(async {
        store
            .run_concurrent(async |access| {
                // The aborted stat never ran against the descriptor now holding the rep.
                assert!(receive.await.is_err());
                let (_task, _leases, receive) = access
                    .with(|mut store| {
                        spawn(
                            store.as_context_mut(),
                            &call("[method]descriptor.stat", &[other]),
                        )
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(receive.await.unwrap().unwrap().bytes[0], 0);
            })
            .await
            .unwrap();
        sync(&mut store, &release_packet(other)).unwrap();
        store.assert_concurrent_state_empty();
    });
}

#[test]
fn unpublished_owned_results_are_closed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("bytes"), [1, 2, 3]).unwrap();
    let mut store = store(directory.path());
    let (root, _) = preopen(&mut store).unwrap();
    let live = store.data().standard.handles.entries.len();
    runtime().block_on(async {
        store
            .run_concurrent(async |access| {
                // The callback owner vanished before delivery: settle closes the descriptor.
                let (task, _leases, receive) = access
                    .with(|mut store| spawn(store.as_context_mut(), &open_at(root, "bytes")))
                    .unwrap()
                    .unwrap();
                drop(receive);
                task.await;
                assert_eq!(
                    access.with(|mut store| store
                        .as_context_mut()
                        .data()
                        .standard
                        .handles
                        .entries
                        .len()),
                    live
                );
                // Delivered to the driver but not to the guest (cancellation won the turn).
                let (_task, _leases, receive) = access
                    .with(|mut store| spawn(store.as_context_mut(), &open_at(root, "bytes")))
                    .unwrap()
                    .unwrap();
                let delivery = receive.await.unwrap().unwrap();
                let [id] = delivery.issued[..] else {
                    panic!("one descriptor")
                };
                let rep = access.with(|mut store| {
                    match store
                        .as_context_mut()
                        .data()
                        .standard
                        .handles
                        .entries
                        .get(&id)
                    {
                        Some(Entry::Resource { rep, .. }) => *rep,
                        _ => panic!("descriptor"),
                    }
                });
                access.with(|mut store| {
                    rollback(store.as_context_mut().data_mut(), delivery.issued).unwrap();
                    assert!(
                        sync(&mut store, &release_packet(id))
                            .unwrap_err()
                            .contains("unknown")
                    );
                    // The upstream descriptor was deleted: its slot is reused next.
                    let (again, again_rep) = preopen(&mut store).unwrap();
                    assert_eq!(again_rep, rep);
                    sync(&mut store, &release_packet(again)).unwrap();
                });
            })
            .await
            .unwrap();
        store.assert_concurrent_state_empty();
    });
}

#[test]
fn dropping_a_stream_settles_its_pending_read_immediately() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let (id, consumer) = store.data_mut().standard.handles.stream(None).unwrap();
    store.data_mut().standard.handles.issued.clear();
    runtime().block_on(async {
        store
            .run_concurrent(async |access| {
                let (_task, _leases, receive) = access
                    .with(|mut store| spawn(store.as_context_mut(), &stream_read(id, 16)))
                    .unwrap()
                    .unwrap();
                let mut receive = std::pin::pin!(receive);
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    futures::poll!(&mut receive).is_pending(),
                    "read waits for bytes"
                );
                access
                    .with(|mut store| sync(&mut store, &release_packet(id)))
                    .unwrap();
                let error = tokio::time::timeout(Duration::from_secs(1), receive)
                    .await
                    .expect("drop wakes the reader")
                    .unwrap()
                    .err()
                    .unwrap();
                assert!(error.contains("dropped"));
            })
            .await
            .unwrap();
    });
    drop(consumer);
}

#[test]
fn a_full_table_refuses_without_leaking_upstream_or_detached_state() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let (first, rep) = preopen(&mut store).unwrap();
    sync(&mut store, &release_packet(first)).unwrap();
    let handles = &mut store.data_mut().standard.handles;
    let mut streams: Vec<_> = (0..HANDLE_LIMIT)
        .map(|_| handles.stream(None).unwrap())
        .collect();
    handles.issued.clear();
    for _ in 0..3 {
        assert!(preopen(&mut store).unwrap_err().contains("table is full"));
    }
    let handles = &store.data().standard.handles;
    assert!(handles.orphans.is_empty() && handles.issued.is_empty() && handles.refusal.is_none());
    // A dropped stream whose producer still holds transmit state keeps counting.
    let (stream, consumer) = streams.remove(0);
    sync(&mut store, &release_packet(stream)).unwrap();
    assert!(preopen(&mut store).unwrap_err().contains("table is full"));
    assert!(sync(&mut store, &release_packet(stream)).is_err());
    drop(consumer);
    // Every refused preopen was closed upstream, so the first freed slot is reused.
    let (again, again_rep) = preopen(&mut store).unwrap();
    assert_eq!(again_rep, rep);
    sync(&mut store, &release_packet(again)).unwrap();
}

#[test]
fn synchronous_stream_creation_retains_its_file_before_the_descriptor_drops() {
    let directory = tempfile::tempdir().unwrap();
    let expected: Vec<u8> = (0..65539).map(|i| (i % 256) as u8).collect();
    std::fs::write(directory.path().join("bytes"), &expected).unwrap();
    let mut store = store(directory.path());
    let (root, _) = preopen(&mut store).unwrap();
    runtime().block_on(async {
        let id = store
            .run_concurrent(async |access| {
                let (_task, _leases, receive) = access
                    .with(|mut store| spawn(store.as_context_mut(), &open_at(root, "bytes")))
                    .unwrap()
                    .unwrap();
                let delivery = receive.await.unwrap().unwrap();
                let mut reader = Reader(&delivery.bytes);
                assert_eq!(reader.u8().unwrap(), 0);
                let id = reader.u64().unwrap();
                reader.end().unwrap();
                id
            })
            .await
            .unwrap();
        let mut request = call("[method]descriptor.read-via-stream", &[id]);
        request.extend_from_slice(&0u64.to_le_bytes());
        let reply = sync(&mut store, &request).unwrap();
        let mut reader = Reader(&reply);
        let stream = reader.u64().unwrap();
        let future = reader.u64().unwrap();
        reader.end().unwrap();
        sync(&mut store, &release_packet(id)).unwrap();
        let mut actual = Vec::new();
        loop {
            let request = stream_read(stream, PIPE_BYTES as u32);
            let bytes = store
                .run_concurrent(async |access| {
                    let (_task, _leases, receive) = access
                        .with(|mut state| spawn(state.as_context_mut(), &request))
                        .unwrap()
                        .unwrap();
                    receive.await.unwrap().unwrap().bytes
                })
                .await
                .unwrap();
            let mut reader = Reader(&bytes);
            if reader.u8().unwrap() == 1 {
                reader.end().unwrap();
                break;
            }
            actual.extend_from_slice(reader.bytes().unwrap());
            reader.end().unwrap();
        }
        assert_eq!(actual, expected);
        let bytes = store
            .run_concurrent(async |access| {
                let (_task, _leases, receive) = access
                    .with(|mut state| spawn(state.as_context_mut(), &future_read(future)))
                    .unwrap()
                    .unwrap();
                receive.await.unwrap().unwrap().bytes
            })
            .await
            .unwrap();
        assert_eq!(bytes, vec![0]);
        sync(&mut store, &release_packet(stream)).unwrap();
        // A consumed future is no longer a guest name.
        assert!(sync(&mut store, &release_packet(future)).is_err());
        sync(&mut store, &release_packet(root)).unwrap();
        store.assert_concurrent_state_empty();
    });
    // Unused directly by the test body but kept to document the upstream types in play.
    let _ = std::marker::PhantomData::<(WasiFilesystem, types::DescriptorFlags)>;
}

#[test]
fn retiring_a_callback_drops_abandoned_admitted_work_and_detached_state() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("bytes"), vec![7u8; 1 << 20]).unwrap();
    let mut store = store(directory.path());
    let (root, _) = preopen(&mut store).unwrap();
    let runtime = runtime();
    let fd = runtime
        .block_on(store.run_concurrent(async |access| {
            let (_task, _leases, receive) = access
                .with(|mut store| spawn(store.as_context_mut(), &open_at(root, "bytes")))
                .unwrap()
                .unwrap();
            let delivery = receive.await.unwrap().unwrap();
            u64::from_le_bytes(delivery.bytes[1..9].try_into().unwrap())
        }))
        .unwrap();
    let mut request = call("[method]descriptor.read-via-stream", &[fd]);
    request.extend_from_slice(&0u64.to_le_bytes());
    let reply = sync(&mut store, &request).unwrap();
    let stream = u64::from_le_bytes(reply[0..8].try_into().unwrap());
    let future = u64::from_le_bytes(reply[8..16].try_into().unwrap());
    // Let the upstream producer fill the pipe and park on backpressure.
    runtime
        .block_on(store.run_concurrent(async |_| {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }))
        .unwrap();
    // A callback abandons admitted calls that never ran and a pending future read.
    let mut tasks = Tasks::default();
    for _ in 0..120 {
        let (task, leases, _) = spawn(
            store.as_context_mut(),
            &call("[method]descriptor.stat", &[root]),
        )
        .unwrap()
        .unwrap();
        tasks.push(task, leases);
    }
    let (task, leases, _) = spawn(store.as_context_mut(), &future_read(future))
        .unwrap()
        .unwrap();
    tasks.push(task, leases);
    assert_eq!(tasks.len(), 121);
    sync(&mut store, &release_packet(stream)).unwrap();
    let detached = |store: &wasmtime::Store<State>| {
        store
            .data()
            .standard
            .handles
            .entries
            .values()
            .filter(|entry| matches!(entry, Entry::Detached(_)))
            .count()
    };
    assert_eq!(detached(&store), 2);
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert_eq!(tasks.len(), 0);
    // No aborted future, reader or stream producer survives in the Store, and the detached
    // stream and future no longer count toward the table.
    assert_eq!(detached(&store), 0);
    store.assert_concurrent_state_empty();
    sync(&mut store, &release_packet(fd)).unwrap();
    sync(&mut store, &release_packet(root)).unwrap();
}

#[test]
fn sequential_calls_do_not_grow_the_callback_task_list() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let mut request = call("wait-for", &[]);
    request.extend_from_slice(&0u64.to_le_bytes());
    let mut tasks = Tasks::default();
    runtime().block_on(async {
        for _ in 0..512 {
            let (task, leases, receive) = spawn(store.as_context_mut(), &request).unwrap().unwrap();
            tasks.push(task, leases);
            assert!(tasks.len() <= 2, "settled tasks are pruned");
            store
                .run_concurrent(async |_| receive.await.unwrap().unwrap())
                .await
                .unwrap();
        }
        let mut drained = std::mem::take(&mut tasks);
        retire(&mut store, &mut drained).await.unwrap();
        store.assert_concurrent_state_empty();
    });
}

/// Byte chunk reply: tag 0, u32 length, bytes.
fn chunk(bytes: &[u8]) -> Vec<u8> {
    let mut reply = vec![0];
    reply.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    reply.extend_from_slice(bytes);
    reply
}

#[test]
fn concurrent_reads_of_one_stream_are_fifo_and_cancel_drop_and_eof_settle_every_reader() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = store(directory.path());
    let (id, consumer) = store.data_mut().standard.handles.stream(None).unwrap();
    let (other, other_consumer) = store.data_mut().standard.handles.stream(None).unwrap();
    store.data_mut().standard.handles.issued.clear();
    let Some(Entry::Stream(pipe)) = store.data().standard.handles.entries.get(&id) else {
        panic!("stream")
    };
    let pipe = Arc::clone(pipe);
    let mut consumer = Some(consumer);
    runtime().block_on(async {
        store
            .run_concurrent(async |access| {
                let read = |stream: u64| {
                    access
                        .with(|mut store| spawn(store.as_context_mut(), &stream_read(stream, 2)))
                        .unwrap()
                        .unwrap()
                };
                let settle = async |receive: Completion| {
                    tokio::time::timeout(Duration::from_secs(1), receive)
                        .await
                        .expect("every queued reader settles")
                        .unwrap()
                        .map(|delivery| delivery.bytes)
                };
                let turns = async || {
                    for _ in 0..4 {
                        tokio::task::yield_now().await;
                    }
                };
                // Four reads pending at once on an empty stream (Promise.all over READ_CHUNK);
                // a queued, never-served read is cancelled without starving later readers.
                let (_a, _, a) = read(id);
                let (b_task, _, b) = read(id);
                let (_c, _, c) = read(id);
                let (_d, _, d) = read(id);
                turns().await;
                b_task.abort();
                turns().await;
                pipe.lock().unwrap().produce(b"abcdef");
                assert_eq!(settle(a).await, Ok(chunk(b"ab")));
                assert_eq!(settle(c).await, Ok(chunk(b"cd")));
                assert_eq!(settle(d).await, Ok(chunk(b"ef")));
                assert!(b.await.is_err(), "the cancelled read has no reply");
                // Cancelling the head read hands its turn to the next reader.
                let (e_task, _, e) = read(id);
                let (_f, _, f) = read(id);
                turns().await;
                e_task.abort();
                turns().await;
                pipe.lock().unwrap().produce(b"gh");
                assert_eq!(settle(f).await, Ok(chunk(b"gh")));
                assert!(e.await.is_err());
                // End of stream settles every queued reader in order.
                let (_g, _, g) = read(id);
                let (_h, _, h) = read(id);
                turns().await;
                drop(consumer.take());
                assert_eq!(settle(g).await, Ok(vec![1]));
                assert_eq!(settle(h).await, Ok(vec![1]));
                // DROP settles every queued reader, not only the last one to park.
                let (_i, _, i) = read(other);
                let (_j, _, j) = read(other);
                turns().await;
                access
                    .with(|mut store| sync(&mut store, &release_packet(other)))
                    .unwrap();
                assert!(settle(i).await.unwrap_err().contains("dropped"));
                assert!(settle(j).await.unwrap_err().contains("dropped"));
            })
            .await
            .unwrap();
    });
    drop(other_consumer);
}
