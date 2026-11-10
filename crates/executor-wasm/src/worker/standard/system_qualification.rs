//! Qualify the generated clock, random and CLI bindings with raw packets against the real
//! upstream implementations, configured through Tembra's provider facade.
use super::*;
use crate::{
    install::{Cli, Input, Output},
    providers::{Providers, System},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Poll, Waker},
    time::Duration,
};

const MONOTONIC: &str = "wasi:clocks/monotonic-clock@0.3.0";
const SYSTEM: &str = "wasi:clocks/system-clock@0.3.0";
const RANDOM: &str = "wasi:random/random@0.3.0";
const INSECURE: &str = "wasi:random/insecure@0.3.0";
const SEED: &str = "wasi:random/insecure-seed@0.3.0";
const ENVIRONMENT: &str = "wasi:cli/environment@0.3.0";
const EXIT: &str = "wasi:cli/exit@0.3.0";
const STDIN: &str = "wasi:cli/stdin@0.3.0";
const STDOUT: &str = "wasi:cli/stdout@0.3.0";
const TERMINAL: &str = "wasi:cli/terminal-stdin@0.3.0";

/// A raw request in the guest's layout; methods are found by interface and name.
struct Packet(Vec<u8>);

impl Packet {
    fn op(operation: u8) -> Self {
        Self(vec![PACKET_TAG, operation])
    }
    fn call(interface: &str, name: &str) -> Self {
        let index = (0..METHOD_NAMES.len())
            .find(|i| INTERFACES[*i] == interface && METHOD_NAMES[*i] == name)
            .unwrap_or_else(|| panic!("{interface}#{name} is not generated"));
        Self::op(CALL).u32(index as u32)
    }
    fn u8(mut self, value: u8) -> Self {
        self.0.push(value);
        self
    }
    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn bytes(mut self, value: &[u8]) -> Self {
        self.0
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.0.extend_from_slice(value);
        self
    }
}

type Store = wasmtime::Store<State>;

fn store(system: System, cli: Cli, interfaces: &[&str]) -> Store {
    let engine = crate::Engine::new(crate::Limits::default()).unwrap();
    let providers = Providers {
        system,
        ..Providers::default()
    };
    let mut prepared = providers.context(&[], &granted(), &cli).unwrap();
    let mut state = super::super::runtime::test_state(prepared.wasi.build());
    state.wasi = super::super::wasi::Wasi::new(prepared.clocks);
    for interface in interfaces {
        state.standard.allowed.insert((*interface).into());
    }
    wasmtime::Store::new(&engine.engine, state)
}

/// Clocks, random and CLI granted; standard bindings still need the Store's `allowed` imports.
fn granted() -> crate::Grants {
    crate::Grants {
        system: [
            crate::SystemCapability::Clocks,
            crate::SystemCapability::Random,
            crate::SystemCapability::Cli,
        ]
        .into(),
        ..Default::default()
    }
}

fn sync(store: &mut Store, packet: &[u8]) -> Result<Vec<u8>, String> {
    synchronous(store.as_context_mut(), packet).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn run(
    runtime: &tokio::runtime::Runtime,
    store: &mut Store,
    packet: &[u8],
) -> Result<Vec<u8>, String> {
    runtime
        .block_on(store.run_concurrent(async |access| {
            let admitted = access.with(|mut store| spawn(store.as_context_mut(), packet));
            match admitted.unwrap() {
                Ok((_task, _leases, receive)) => receive.await.unwrap().map(|done| done.bytes),
                Err(error) => Err(error),
            }
        }))
        .unwrap()
}

struct Wall;
impl crate::HostWallClock for Wall {
    fn resolution(&self) -> Duration {
        Duration::from_nanos(1)
    }
    fn now(&self) -> Duration {
        Duration::new(1_700_000_000, 123)
    }
}
struct Monotonic;
impl crate::HostMonotonicClock for Monotonic {
    fn resolution(&self) -> u64 {
        7
    }
    fn now(&self) -> u64 {
        42
    }
}

const SEED_VALUE: u128 = 0x0102_0304_0506_0708_1112_1314_1516_1718;

fn deterministic() -> System {
    System {
        wall: Some(std::sync::Arc::new(|| Box::new(Wall))),
        monotonic: Some(std::sync::Arc::new(|| Box::new(Monotonic))),
        secure: Some(std::sync::Arc::new(|context| {
            context.secure_random(crate::Deterministic::new(vec![1, 2, 3]));
        })),
        insecure: Some(std::sync::Arc::new(|context| {
            context.insecure_random(crate::Deterministic::new(vec![9]));
        })),
        seed: Some(std::sync::Arc::new(|| SEED_VALUE)),
    }
}

#[test]
fn clocks_and_random_read_the_configured_sources_exactly() {
    let interfaces = [MONOTONIC, SYSTEM, RANDOM, INSECURE, SEED];
    let mut store = store(deterministic(), Cli::default(), &interfaces);
    let now = sync(&mut store, &Packet::call(SYSTEM, "now").0).unwrap();
    assert_eq!(
        i64::from_le_bytes(now[..8].try_into().unwrap()),
        1_700_000_000
    );
    assert_eq!(u32::from_le_bytes(now[8..].try_into().unwrap()), 123);
    assert_eq!(
        sync(&mut store, &Packet::call(SYSTEM, "get-resolution").0).unwrap(),
        1u64.to_le_bytes()
    );
    assert_eq!(
        sync(&mut store, &Packet::call(MONOTONIC, "now").0).unwrap(),
        42u64.to_le_bytes()
    );
    assert_eq!(
        sync(&mut store, &Packet::call(MONOTONIC, "get-resolution").0).unwrap(),
        7u64.to_le_bytes()
    );
    let secure = sync(
        &mut store,
        &Packet::call(RANDOM, "get-random-bytes").u64(5).0,
    )
    .unwrap();
    assert_eq!(secure, [&5u32.to_le_bytes()[..], &[1, 2, 3, 1, 2]].concat());
    let insecure = sync(
        &mut store,
        &Packet::call(INSECURE, "get-insecure-random-bytes").u64(3).0,
    )
    .unwrap();
    assert_eq!(insecure, [&3u32.to_le_bytes()[..], &[9, 9, 9]].concat());
    let seed = sync(&mut store, &Packet::call(SEED, "get-insecure-seed").0).unwrap();
    assert_eq!(seed[..8], (SEED_VALUE as u64).to_le_bytes());
    assert_eq!(seed[8..], ((SEED_VALUE >> 64) as u64).to_le_bytes());
    // Insecure byte requests are capped to one packet like secure ones.
    let capped = sync(
        &mut store,
        &Packet::call(INSECURE, "get-insecure-random-bytes")
            .u64(u64::MAX)
            .0,
    )
    .unwrap();
    assert_eq!(capped.len(), store.data().packet_bytes);
    // The built-in seed is the same per-Store value as the standard insecure seed.
    let providers = Providers {
        system: deterministic(),
        ..Providers::default()
    };
    let grants = crate::Grants {
        system: [crate::SystemCapability::Random].into(),
        ..Default::default()
    };
    let builtins = providers
        .context(&[], &grants, &Cli::default())
        .unwrap()
        .builtins;
    assert!(!builtins.clock);
    assert_eq!(
        builtins.seed,
        Some([
            (SEED_VALUE as u64).to_string(),
            ((SEED_VALUE >> 64) as u64).to_string()
        ])
    );
    let none = providers
        .context(&[], &crate::Grants::default(), &Cli::default())
        .unwrap();
    assert_eq!(none.builtins, Default::default());
    assert!(none.clocks.is_none());
}

/// Stateful providers: every factory call yields a distinguishable instance or value.
fn counted(walls: &'static AtomicUsize, seeds: &'static AtomicUsize) -> System {
    struct Numbered(u64);
    impl crate::HostWallClock for Numbered {
        fn resolution(&self) -> Duration {
            Duration::from_nanos(1)
        }
        fn now(&self) -> Duration {
            Duration::from_secs(1000 * self.0)
        }
    }
    System {
        wall: Some(std::sync::Arc::new(move || {
            Box::new(Numbered(walls.fetch_add(1, Ordering::SeqCst) as u64 + 1))
        })),
        seed: Some(std::sync::Arc::new(move || {
            0xabc0 + seeds.fetch_add(1, Ordering::SeqCst) as u128 + (7u128 << 64)
        })),
        ..System::default()
    }
}

#[test]
fn one_clock_pair_and_seed_per_store_serve_both_wasi_and_builtins() {
    static WALLS: AtomicUsize = AtomicUsize::new(0);
    static SEEDS: AtomicUsize = AtomicUsize::new(0);
    let providers = Providers {
        system: counted(&WALLS, &SEEDS),
        ..Providers::default()
    };
    for store_number in 1..=2u64 {
        let mut prepared = providers.context(&[], &granted(), &Cli::default()).unwrap();
        // Exactly one factory call per new Store, shared by every consumer.
        assert_eq!(WALLS.load(Ordering::SeqCst) as u64, store_number);
        assert_eq!(SEEDS.load(Ordering::SeqCst) as u64, store_number);
        let builtins = prepared.builtins.clone();
        let mut state = super::super::runtime::test_state(prepared.wasi.build());
        state.wasi = super::super::wasi::Wasi::new(prepared.clocks.take());
        for interface in [SYSTEM, SEED] {
            state.standard.allowed.insert(interface.into());
        }
        let engine = crate::Engine::new(crate::Limits::default()).unwrap();
        let mut store = wasmtime::Store::new(&engine.engine, state);
        // The interpreter's WASIp1 clock and the standard clock read the same instance.
        let mut memory = vec![0u8; 8];
        assert_eq!(
            store
                .data()
                .wasi
                .clock_time_get(&mut memory, crate::abi::CLOCK_REALTIME, 0),
            0
        );
        let libc = u64::from_le_bytes(memory.try_into().unwrap());
        let now = sync(&mut store, &Packet::call(SYSTEM, "now").0).unwrap();
        let seconds = i64::from_le_bytes(now[..8].try_into().unwrap()) as u64;
        assert_eq!(seconds, 1000 * store_number);
        assert_eq!(libc, seconds * 1_000_000_000);
        // The built-in seed is the standard insecure seed of the same Store.
        let seed = sync(&mut store, &Packet::call(SEED, "get-insecure-seed").0).unwrap();
        let low = u64::from_le_bytes(seed[..8].try_into().unwrap());
        let high = u64::from_le_bytes(seed[8..].try_into().unwrap());
        assert_eq!(builtins.seed, Some([low.to_string(), high.to_string()]));
        assert_eq!(low, 0xabc0 + store_number - 1);
        assert_eq!(high, 7);
    }
}

#[test]
fn without_grants_no_configured_provider_or_cli_file_is_touched() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    struct Never;
    impl crate::HostWallClock for Never {
        fn resolution(&self) -> Duration {
            Duration::ZERO
        }
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }
    impl crate::HostMonotonicClock for Never {
        fn resolution(&self) -> u64 {
            0
        }
        fn now(&self) -> u64 {
            0
        }
    }
    let count = || {
        CALLS.fetch_add(1, Ordering::SeqCst);
    };
    let system = System {
        wall: Some(std::sync::Arc::new(move || {
            count();
            Box::new(Never)
        })),
        monotonic: Some(std::sync::Arc::new(move || {
            count();
            Box::new(Never)
        })),
        secure: Some(std::sync::Arc::new(move |_| count())),
        insecure: Some(std::sync::Arc::new(move |_| count())),
        seed: Some(std::sync::Arc::new(move || {
            count();
            1
        })),
    };
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("never-created");
    let cli = Cli {
        stdin: Input::Reader(std::sync::Arc::new(move || {
            count();
            Box::new(std::io::Cursor::new(Vec::new()))
        })),
        stdout: Output::File(output.clone()),
        stderr: Output::File(output.clone()),
        ..Cli::default()
    };
    let providers = Providers {
        system,
        ..Providers::default()
    };
    let prepared = providers
        .context(&[], &crate::Grants::default(), &cli)
        .unwrap();
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    assert!(!output.exists());
    assert!(prepared.clocks.is_none());
    assert_eq!(prepared.builtins, Default::default());
    // The CLI grant alone reaches only CLI data: the file opens, clock/random stay untouched.
    let cli_only = crate::Grants {
        system: [crate::SystemCapability::Cli].into(),
        ..Default::default()
    };
    providers.context(&[], &cli_only, &cli).unwrap();
    assert!(output.exists());
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "only the stdin reader factory ran"
    );
}

#[test]
fn waits_finish_or_abort_with_their_callback() {
    let runtime = runtime();
    let mut store = store(System::default(), Cli::default(), &[MONOTONIC]);
    // A mark already reached completes at once.
    let reached = run(
        &runtime,
        &mut store,
        &Packet::call(MONOTONIC, "wait-until").u64(0).0,
    );
    assert_eq!(reached, Ok(Vec::new()));
    let mut tasks = Tasks::default();
    let (task, leases, receive) = spawn(
        store.as_context_mut(),
        &Packet::call(MONOTONIC, "wait-for").u64(60_000_000_000).0,
    )
    .unwrap()
    .unwrap();
    tasks.push(task, leases);
    let started = std::time::Instant::now();
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        runtime.block_on(receive).is_err(),
        "the revoked wait never replies"
    );
    store.assert_concurrent_state_empty();
}

#[test]
fn ungranted_interfaces_refuse_before_any_provider_access() {
    static READS: AtomicUsize = AtomicUsize::new(0);
    struct Counted;
    impl crate::HostWallClock for Counted {
        fn resolution(&self) -> Duration {
            Duration::from_nanos(1)
        }
        fn now(&self) -> Duration {
            READS.fetch_add(1, Ordering::SeqCst);
            Duration::ZERO
        }
    }
    let system = System {
        wall: Some(std::sync::Arc::new(|| Box::new(Counted))),
        ..System::default()
    };
    let mut store = store(system, Cli::default(), &[RANDOM]);
    for (interface, name) in [
        (SYSTEM, "now"),
        (ENVIRONMENT, "get-environment"),
        (EXIT, "exit-with-code"),
    ] {
        let mut packet = Packet::call(interface, name);
        if name == "exit-with-code" {
            packet = packet.u8(1);
        }
        let refused = sync(&mut store, &packet.0).unwrap_err();
        assert!(refused.contains("not imported"), "{interface}: {refused}");
    }
    assert_eq!(READS.load(Ordering::SeqCst), 0);
}

/// `list<string>` or `list<tuple<string, string>>` reply items, and an `option<string>`.
fn strings(reply: &[u8]) -> Vec<String> {
    let mut reader = Reader(reply);
    let mut items = Vec::new();
    while !reader.0.is_empty() {
        items.push(reader.string().unwrap());
    }
    items
}

#[test]
fn cli_data_is_empty_by_default_and_exact_when_configured() {
    assert!(
        std::env::vars().count() > 0,
        "the host process has an environment"
    );
    let interfaces = [ENVIRONMENT];
    let mut empty = store(System::default(), Cli::default(), &interfaces);
    assert_eq!(
        sync(&mut empty, &Packet::call(ENVIRONMENT, "get-environment").0).unwrap(),
        0u32.to_le_bytes()
    );
    assert_eq!(
        sync(&mut empty, &Packet::call(ENVIRONMENT, "get-arguments").0).unwrap(),
        0u32.to_le_bytes()
    );
    assert_eq!(
        sync(&mut empty, &Packet::call(ENVIRONMENT, "get-initial-cwd").0).unwrap(),
        [0]
    );
    let cli = Cli {
        arguments: vec!["orb".into(), "--flag=ü".into()],
        environment: vec![("KEY".into(), "va=lue".into())],
        initial_cwd: Some("/work".into()),
        ..Cli::default()
    };
    let mut configured = store(System::default(), cli, &interfaces);
    let arguments = sync(
        &mut configured,
        &Packet::call(ENVIRONMENT, "get-arguments").0,
    )
    .unwrap();
    assert_eq!(u32::from_le_bytes(arguments[..4].try_into().unwrap()), 2);
    assert_eq!(strings(&arguments[4..]), ["orb", "--flag=ü"]);
    let environment = sync(
        &mut configured,
        &Packet::call(ENVIRONMENT, "get-environment").0,
    )
    .unwrap();
    assert_eq!(strings(&environment[4..]), ["KEY", "va=lue"]);
    let cwd = sync(
        &mut configured,
        &Packet::call(ENVIRONMENT, "get-initial-cwd").0,
    )
    .unwrap();
    assert_eq!(cwd[0], 1);
    assert_eq!(strings(&cwd[1..]), ["/work"]);
}

fn read_all(runtime: &tokio::runtime::Runtime, store: &mut Store, stream: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let reply = run(
            runtime,
            store,
            &Packet::op(STREAM_READ).u64(stream).u32(1 << 16).0,
        )
        .unwrap();
        let mut reader = Reader(&reply);
        if reader.u8().unwrap() == 1 {
            return bytes;
        }
        bytes.extend_from_slice(reader.bytes().unwrap());
    }
}

#[test]
fn stdin_bytes_and_files_stream_exactly() {
    let runtime = runtime();
    let data: Vec<u8> = (0..70_000u32).map(|i| (i * 31 % 256) as u8).collect();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), &data[..1000]).unwrap();
    for (input, expected) in [
        (Input::Bytes(data.clone()), &data[..]),
        (Input::File(directory.path().join("input")), &data[..1000]),
        (Input::Empty, &[][..]),
    ] {
        let cli = Cli {
            stdin: input,
            ..Cli::default()
        };
        let mut store = store(System::default(), cli, &[STDIN]);
        let reply = sync(&mut store, &Packet::call(STDIN, "read-via-stream").0).unwrap();
        let stream = u64::from_le_bytes(reply[..8].try_into().unwrap());
        let done = u64::from_le_bytes(reply[8..].try_into().unwrap());
        assert_eq!(read_all(&runtime, &mut store, stream), expected);
        assert_eq!(
            run(&runtime, &mut store, &Packet::op(FUTURE_READ).u64(done).0).unwrap(),
            [0]
        );
        sync(&mut store, &Packet::op(DROP).u64(stream).0).unwrap();
        runtime
            .block_on(retire(&mut store, &mut Tasks::default()))
            .unwrap();
        store.assert_concurrent_state_empty();
    }
}

/// Gate state: open, bytes written, and the stalled writer's waker.
type GateState = (bool, Vec<u8>, Option<Waker>);

/// A writer that stays physically stalled until its gate opens.
#[derive(Clone, Default)]
struct Gate(std::sync::Arc<std::sync::Mutex<GateState>>);

impl Gate {
    fn open(&self) {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        if let Some(waker) = state.2.take() {
            waker.wake();
        }
    }
    fn written(&self) -> usize {
        self.0.lock().unwrap().1.len()
    }
}

impl tokio::io::AsyncWrite for Gate {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        if !state.0 {
            state.2 = Some(context.waker().clone());
            return Poll::Pending;
        }
        state.1.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn detached(store: &Store) -> usize {
    store
        .data()
        .standard
        .handles
        .entries
        .values()
        .filter(|entry| matches!(entry, Entry::Detached(_)))
        .count()
}

#[test]
fn a_stalled_stdout_keeps_backpressure_and_settles_physically_after_retire() {
    let runtime = runtime();
    let gate = Gate::default();
    let writer = gate.clone();
    let cli = Cli {
        stdout: Output::Writer(std::sync::Arc::new(move || Box::new(writer.clone()))),
        ..Cli::default()
    };
    let mut store = store(System::default(), cli, &[STDOUT]);
    let input = u64::from_le_bytes(
        sync(&mut store, &Packet::op(STREAM_NEW).u32(BYTE_STREAM).0)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let done = u64::from_le_bytes(
        sync(
            &mut store,
            &Packet::call(STDOUT, "write-via-stream").u64(input).0,
        )
        .unwrap()
        .try_into()
        .unwrap(),
    );
    let chunk = vec![0xa5; PIPE_BYTES];
    // The writer is stalled: one chunk is in flight upstream and one more is buffered.
    run(
        &runtime,
        &mut store,
        &Packet::op(STREAM_WRITE).u64(input).bytes(&chunk).0,
    )
    .unwrap();
    run(
        &runtime,
        &mut store,
        &Packet::op(STREAM_WRITE).u64(input).bytes(&chunk).0,
    )
    .unwrap();
    let mut tasks = Tasks::default();
    let (task, leases, receive) = spawn(
        store.as_context_mut(),
        &Packet::op(STREAM_WRITE).u64(input).bytes(&chunk).0,
    )
    .unwrap()
    .unwrap();
    tasks.push(task, leases);
    // Callback end: the third, unaccepted chunk is aborted; the writer closes, but its
    // accepted bytes stay physically owned (counted) while the sink is stalled.
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert!(runtime.block_on(receive).is_err());
    assert_eq!(gate.written(), 0);
    assert_eq!(
        detached(&store),
        1,
        "bounded polls never abandon in-flight output"
    );
    // Once the sink drains, the accepted bytes are delivered and the state is released.
    gate.open();
    for _ in 0..4 {
        runtime
            .block_on(retire(&mut store, &mut Tasks::default()))
            .unwrap();
    }
    assert_eq!(gate.written(), 2 * PIPE_BYTES);
    assert_eq!(detached(&store), 0);
    assert_eq!(
        run(&runtime, &mut store, &Packet::op(FUTURE_READ).u64(done).0).unwrap(),
        [0]
    );
    store.assert_concurrent_state_empty();
}

#[test]
fn stdout_files_receive_exact_bytes_appended() {
    let runtime = runtime();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("out");
    std::fs::write(&path, b"x").unwrap();
    let cli = Cli {
        stdout: Output::File(path.clone()),
        ..Cli::default()
    };
    let mut store = store(System::default(), cli, &[STDOUT]);
    let input = u64::from_le_bytes(
        sync(&mut store, &Packet::op(STREAM_NEW).u32(BYTE_STREAM).0)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let done = u64::from_le_bytes(
        sync(
            &mut store,
            &Packet::call(STDOUT, "write-via-stream").u64(input).0,
        )
        .unwrap()
        .try_into()
        .unwrap(),
    );
    run(
        &runtime,
        &mut store,
        &Packet::op(STREAM_WRITE)
            .u64(input)
            .bytes(&[0xff, 0, 0x80])
            .0,
    )
    .unwrap();
    sync(&mut store, &Packet::op(DROP).u64(input).0).unwrap();
    assert_eq!(
        run(&runtime, &mut store, &Packet::op(FUTURE_READ).u64(done).0).unwrap(),
        [0]
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"x\xff\x00\x80");
}

#[test]
fn exit_is_a_contained_failure_with_its_exact_status() {
    let mut store = store(System::default(), Cli::default(), &[EXIT]);
    for (packet, status) in [
        (Packet::call(EXIT, "exit-with-code").u8(7), 7),
        (Packet::call(EXIT, "exit").u8(1), 1),
        (Packet::call(EXIT, "exit").u8(0), 0),
    ] {
        let error = synchronous(store.as_context_mut(), &packet.0).unwrap_err();
        assert_eq!(crate::providers::exit_status(&error), Some(status));
    }
    // An unknown result tag is an ordinary refusal, not an exit.
    assert!(
        sync(&mut store, &Packet::call(EXIT, "exit").u8(2).0)
            .unwrap_err()
            .contains("unknown result tag")
    );
}

#[test]
fn terminals_are_honestly_absent_and_never_spend_file_budget() {
    let cli = Cli {
        stdin: Input::Bytes(vec![1]),
        ..Cli::default()
    };
    let mut store = store(System::default(), cli, &[TERMINAL]);
    let permits = store.data().standard.handles.share.available_permits();
    assert_eq!(
        sync(&mut store, &Packet::call(TERMINAL, "get-terminal-stdin").0).unwrap(),
        [0]
    );
    // Per-kind budgeting: descriptors (kind 0) hold a permit, terminal handles never do.
    assert_eq!(BUDGETED, &[true, false, false, false, false, false, false]);
    let handles = &mut store.data_mut().standard.handles;
    let terminal = handles.own(1, wasmtime::component::Resource::<()>::new_own(5));
    assert_ne!(terminal, 0);
    assert_eq!(handles.share.available_permits(), permits);
    let descriptor = handles.own(0, wasmtime::component::Resource::<()>::new_own(6));
    assert_ne!(descriptor, 0);
    assert_eq!(handles.share.available_permits(), permits - 1);
    handles.entries.clear();
    handles.issued.clear();
    assert_eq!(handles.share.available_permits(), permits);
}

#[test]
fn unstable_timezone_is_not_bound() {
    assert!(
        INTERFACES
            .iter()
            .all(|interface| !interface.contains("timezone"))
    );
    let mut store = store(
        System::default(),
        Cli::default(),
        &["wasi:clocks/timezone@0.3.0"],
    );
    let refused = sync(
        &mut store,
        &Packet::op(CALL).u32(METHOD_NAMES.len() as u32).0,
    )
    .unwrap_err();
    assert!(refused.contains("unknown standard method"));
}
