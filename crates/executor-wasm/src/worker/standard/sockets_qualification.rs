//! Qualify standard `wasi:sockets` on the source path with raw packets in the guest layout,
//! against real loopback peers: grants scoped to the bound provider, TCP client and server
//! streaming, the private stream adapter's lifecycle (idle close, backpressure, reopen, accept
//! ownership, traps), UDP and tracked name lookup.
use super::*;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    thread,
    time::Duration,
};
use tembra_contracts::{HostImport, NetworkGrant};
use wasmtime::Store;

const TYPES: &str = "wasi:sockets/types@0.3.0";
const LOOKUP: &str = "wasi:sockets/ip-name-lookup@0.3.0";
/// `error-code` cases of `wasi:sockets/types`.
const ACCESS_DENIED: u32 = 0;
const BOUND: Duration = Duration::from_secs(10);

/// A raw request in the guest's layout; methods are found by interface and name.
struct P(Vec<u8>);

impl P {
    fn op(operation: u8) -> Self {
        Self(vec![PACKET_TAG, operation])
    }
    fn call(interface: &str, name: &str) -> Self {
        let index = (0..METHOD_NAMES.len())
            .find(|i| INTERFACES[*i] == interface && METHOD_NAMES[*i] == name)
            .unwrap_or_else(|| panic!("{interface}#{name} is not generated"));
        Self::op(CALL).u32(index as u32)
    }
    fn tcp(name: &str) -> Self {
        Self::call(TYPES, name)
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
    /// An IPv4 `ip-socket-address`.
    fn address(self, address: SocketAddr) -> Self {
        let SocketAddr::V4(address) = address else {
            panic!("IPv4 only")
        };
        let mut packet = self.u32(0);
        packet.0.extend_from_slice(&address.port().to_le_bytes());
        packet.0.extend_from_slice(&address.ip().octets());
        packet
    }
}

fn id(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

/// A Store whose `wasi:sockets/types` import is bound to provider `net` with `grant`, and whose
/// `ip-name-lookup` import is bound to provider `dns`. Each provider's grant is scoped to its own
/// interface: `dns` also grants a TCP connect and `net` may grant resolution, and neither widens
/// the other interface.
fn scoped(grant: NetworkGrant, resolve: bool) -> Store<State> {
    // `TEMBRA_TEST_PULLEY=1` runs the same qualification (and the adapter) on Pulley.
    let engine = if std::env::var_os("TEMBRA_TEST_PULLEY").is_some() {
        crate::Engine::interpreted(crate::Limits::default())
    } else {
        crate::Engine::new(crate::Limits::default())
    }
    .unwrap();
    let providers = crate::providers::Providers {
        network: ["net".to_owned(), "dns".to_owned()].into(),
        ..Default::default()
    };
    let import = |contract: &str, provider: &str| HostImport {
        binding: contract.into(),
        contract: contract.into(),
        provider: provider.into(),
    };
    let lookup = NetworkGrant {
        provider: "dns".into(),
        connect: vec!["127.0.0.1:9".parse().unwrap()],
        resolve,
        ..Default::default()
    };
    let grants = crate::Grants {
        network: vec![
            NetworkGrant {
                provider: "net".into(),
                ..grant
            },
            lookup,
        ],
        ..Default::default()
    };
    let mut prepared = providers
        .context(
            &[import(TYPES, "net"), import(LOOKUP, "dns")],
            &grants,
            &crate::Cli::default(),
        )
        .unwrap();
    let mut state = super::super::runtime::test_state(prepared.wasi.build());
    state.resolve = prepared.resolve;
    state.standard.allowed.insert(TYPES.into());
    state.standard.allowed.insert(LOOKUP.into());
    let mut store = Store::new(&engine.engine, state);
    store.set_fuel(crate::Limits::default().fuel).unwrap();
    store.fuel_async_yield_interval(Some(10_000)).unwrap();
    store
}

fn sockets(grant: NetworkGrant) -> Store<State> {
    scoped(grant, false)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn sync(store: &mut Store<State>, packet: &P) -> Result<Vec<u8>, String> {
    synchronous(store.as_context_mut(), &packet.0)
        .await
        .unwrap()
}

/// Admit one async packet as a task of the current callback.
fn admit(store: &mut Store<State>, packet: &P, tasks: &mut Tasks) -> Completion {
    let (task, leases, receive) = spawn(store.as_context_mut(), &packet.0).unwrap().unwrap();
    tasks.push(task, leases);
    receive
}

/// Run one Store turn (pumps started first, as the step driver does) until `receive` settles.
async fn turn(store: &mut Store<State>, receive: Completion) -> Result<Delivery, String> {
    start_streams(store).await.unwrap();
    store
        .run_concurrent(async |_| tokio::time::timeout(BOUND, receive).await)
        .await
        .unwrap()
        .expect("turn settles within its bound")
        .unwrap()
}

async fn call(store: &mut Store<State>, packet: &P, tasks: &mut Tasks) -> Result<Vec<u8>, String> {
    let receive = admit(store, packet, tasks);
    turn(store, receive).await.map(|delivery| delivery.bytes)
}

async fn read(store: &mut Store<State>, stream: u64, limit: u32, tasks: &mut Tasks) -> Delivery {
    let receive = admit(store, &P::op(STREAM_READ).u64(stream).u32(limit), tasks);
    turn(store, receive).await.unwrap()
}

/// Read a byte stream until `count` bytes arrived or it ended.
async fn read_bytes(
    store: &mut Store<State>,
    stream: u64,
    count: usize,
    tasks: &mut Tasks,
) -> Vec<u8> {
    let mut data = Vec::new();
    while data.len() < count {
        let reply = read(store, stream, 1 << 16, tasks).await.bytes;
        if reply == [1] {
            break;
        }
        assert_eq!(reply[0], 0);
        data.extend_from_slice(&reply[5..]);
    }
    data
}

async fn drop_handle(store: &mut Store<State>, handle: u64) {
    sync(store, &P::op(DROP).u64(handle)).await.unwrap();
}

/// A connected TCP socket handle.
async fn connect(store: &mut Store<State>, peer: SocketAddr, tasks: &mut Tasks) -> u64 {
    let created = sync(store, &P::tcp("[static]tcp-socket.create").u32(0))
        .await
        .unwrap();
    assert_eq!(created[0], 0, "create succeeds");
    let socket = id(&created[1..]);
    let connected = call(
        store,
        &P::tcp("[method]tcp-socket.connect")
            .u64(socket)
            .address(peer),
        tasks,
    )
    .await
    .unwrap();
    assert_eq!(connected, [0], "connect succeeds");
    socket
}

/// `(stream, completion future)` of `receive`.
async fn receive(store: &mut Store<State>, socket: u64) -> (u64, u64) {
    let reply = sync(store, &P::tcp("[method]tcp-socket.receive").u64(socket))
        .await
        .unwrap();
    (id(&reply), id(&reply[8..]))
}

/// A listening socket on `127.0.0.1:0` and its listen stream.
async fn listen(store: &mut Store<State>) -> (u64, u64, SocketAddr) {
    let created = sync(store, &P::tcp("[static]tcp-socket.create").u32(0))
        .await
        .unwrap();
    let socket = id(&created[1..]);
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    assert_eq!(
        sync(
            store,
            &P::tcp("[method]tcp-socket.bind").u64(socket).address(local)
        )
        .await
        .unwrap(),
        [0]
    );
    let listened = sync(store, &P::tcp("[method]tcp-socket.listen").u64(socket))
        .await
        .unwrap();
    assert_eq!(listened[0], 0, "listen succeeds: {listened:?}");
    let address = sync(
        store,
        &P::tcp("[method]tcp-socket.get-local-address").u64(socket),
    )
    .await
    .unwrap();
    let port = u16::from_le_bytes([address[5], address[6]]);
    (
        socket,
        id(&listened[1..]),
        SocketAddr::from(([127, 0, 0, 1], port)),
    )
}

fn closing_pumps(store: &Store<State>) -> usize {
    store.data().standard.adapter.closing.live()
}

/// Whether the peer observes EOF (or a reset) within the bound.
fn closed_by_host(peer: &mut TcpStream) -> bool {
    peer.set_read_timeout(Some(BOUND)).unwrap();
    let mut byte = [0; 1];
    match peer.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

fn loopback_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

fn connect_grant(peer: SocketAddr) -> NetworkGrant {
    NetworkGrant {
        connect: vec![peer],
        ..Default::default()
    }
}

fn listen_grant() -> NetworkGrant {
    NetworkGrant {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    }
}

#[test]
fn ungranted_connects_are_denied_before_any_side_effect() {
    let (listener, peer) = loopback_listener();
    listener.set_nonblocking(true).unwrap();
    // Only `dns` grants a connect (to 127.0.0.1:9); `net`, bound to the socket types, grants
    // nothing here.
    let mut store = sockets(NetworkGrant::default());
    runtime().block_on(async {
        let created = sync(&mut store, &P::tcp("[static]tcp-socket.create").u32(0))
            .await
            .unwrap();
        // No TCP authority at all for the bound provider: creation itself is refused.
        assert_eq!(created[0], 1);
        assert_eq!(
            u32::from_le_bytes(created[1..5].try_into().unwrap()),
            ACCESS_DENIED
        );
    });
    // `net` may connect elsewhere only; `peer` is granted to nobody.
    let mut store = sockets(connect_grant("127.0.0.1:1".parse().unwrap()));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let created = sync(&mut store, &P::tcp("[static]tcp-socket.create").u32(0))
            .await
            .unwrap();
        let socket = id(&created[1..]);
        let refused = call(
            &mut store,
            &P::tcp("[method]tcp-socket.connect")
                .u64(socket)
                .address(peer),
            &mut tasks,
        )
        .await
        .unwrap();
        assert_eq!(refused[0], 1);
        assert_eq!(
            u32::from_le_bytes(refused[1..5].try_into().unwrap()),
            ACCESS_DENIED
        );
    });
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "a denied connect never reaches the peer"
    );
}

#[test]
fn a_client_streams_three_mebibytes_each_way() {
    const SIZE: usize = 3 << 20;
    let (listener, peer) = loopback_listener();
    let echo = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(BOUND)).unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).unwrap();
        let reply: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        stream.write_all(&reply).unwrap();
        received
    });
    let mut store = sockets(connect_grant(peer));
    let received = runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let writer = id(&sync(&mut store, &P::op(STREAM_NEW).u32(BYTE_STREAM))
            .await
            .unwrap());
        let sent = sync(
            &mut store,
            &P::tcp("[method]tcp-socket.send").u64(socket).u64(writer),
        )
        .await
        .unwrap();
        let sent = id(&sent);
        let data: Vec<u8> = (0..SIZE).map(|i| (i % 241) as u8).collect();
        for chunk in data.chunks(1 << 16) {
            call(
                &mut store,
                &P::op(STREAM_WRITE).u64(writer).bytes(chunk),
                &mut tasks,
            )
            .await
            .unwrap();
        }
        drop_handle(&mut store, writer).await;
        let result = call(&mut store, &P::op(FUTURE_READ).u64(sent), &mut tasks)
            .await
            .unwrap();
        assert_eq!(result, [0], "send completes");
        let (stream, _) = receive(&mut store, socket).await;
        let echoed = read_bytes(&mut store, stream, SIZE, &mut tasks).await;
        assert_eq!(echoed.len(), SIZE);
        assert!(
            echoed
                .iter()
                .enumerate()
                .all(|(i, byte)| *byte == (i % 251) as u8)
        );
        retire(&mut store, &mut tasks).await.unwrap();
        echoed.len()
    });
    assert_eq!(received, SIZE);
    let sent = echo.join().unwrap();
    assert_eq!(sent.len(), SIZE);
    assert!(
        sent.iter()
            .enumerate()
            .all(|(i, byte)| *byte == (i % 241) as u8)
    );
}

#[test]
fn a_server_accepts_through_its_pump_and_reads_each_client() {
    let mut store = sockets(listen_grant());
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let (_socket, accepts, address) = listen(&mut store).await;
        let clients: Vec<_> = (0..3u8)
            .map(|index| {
                thread::spawn(move || {
                    let mut client = TcpStream::connect(address).unwrap();
                    client.write_all(&[index; 4]).unwrap();
                    client.shutdown(std::net::Shutdown::Write).unwrap();
                    client
                })
            })
            .collect();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let delivery = read(&mut store, accepts, 8, &mut tasks).await;
            // One owned socket per item, published with this reply.
            assert_eq!(delivery.bytes[0], 0);
            assert_eq!(
                u32::from_le_bytes(delivery.bytes[1..5].try_into().unwrap()),
                1
            );
            let accepted = id(&delivery.bytes[5..]);
            assert_eq!(delivery.issued, vec![accepted]);
            let (stream, _) = receive(&mut store, accepted).await;
            let data = read_bytes(&mut store, stream, 4, &mut tasks).await;
            seen.push(data[0]);
        }
        seen.sort();
        assert_eq!(seen, [0, 1, 2]);
        for client in clients {
            client.join().unwrap();
        }
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

/// Dropping a socket releases only its reference. Its receive stream and its listen
/// stream keep working, and accepted children are independent of the listener socket.
#[test]
fn a_socket_drop_alone_keeps_its_streams_and_children() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(NetworkGrant {
        connect: vec![peer],
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        ..Default::default()
    });
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let (mut remote, _) = listener.accept().unwrap();
        let (stream, _) = receive(&mut store, socket).await;
        drop_handle(&mut store, socket).await;
        remote.write_all(b"after socket drop").unwrap();
        let data = read_bytes(&mut store, stream, 17, &mut tasks).await;
        assert_eq!(data, b"after socket drop");

        let (listening, accepts, address) = listen(&mut store).await;
        drop_handle(&mut store, listening).await;
        let client = thread::spawn(move || {
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(b"child").unwrap();
            client
        });
        let delivery = read(&mut store, accepts, 1, &mut tasks).await;
        let child = id(&delivery.bytes[5..]);
        // Dropping the listen stream closes the listener; the accepted child is independent.
        drop_handle(&mut store, accepts).await;
        let (child_stream, _) = receive(&mut store, child).await;
        assert_eq!(
            read_bytes(&mut store, child_stream, 5, &mut tasks).await,
            b"child"
        );
        let _client = client.join().unwrap();
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

/// The inverse: dropping the receive stream while the socket is retained closes only that
/// direction; sending keeps working. Then a retained idle stream closes physically when the
/// guest finally drops it inside a callback: retire waits for the pump's close, and the peer
/// sees end of stream before the next callback.
#[test]
fn stream_drops_close_their_direction_and_idle_streams_physically() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let (mut remote, _) = listener.accept().unwrap();
        let (stream, _) = receive(&mut store, socket).await;
        remote.write_all(b"x").unwrap();
        assert_eq!(read_bytes(&mut store, stream, 1, &mut tasks).await, b"x");
        drop_handle(&mut store, stream).await;
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(closing_pumps(&store), 0);
        // The write direction is unaffected.
        let writer = id(&sync(&mut store, &P::op(STREAM_NEW).u32(BYTE_STREAM))
            .await
            .unwrap());
        sync(
            &mut store,
            &P::tcp("[method]tcp-socket.send").u64(socket).u64(writer),
        )
        .await
        .unwrap();
        call(
            &mut store,
            &P::op(STREAM_WRITE).u64(writer).bytes(b"still sending"),
            &mut tasks,
        )
        .await
        .unwrap();
        drop_handle(&mut store, writer).await;
        retire(&mut store, &mut tasks).await.unwrap();
        let mut buffer = [0; 13];
        remote.set_read_timeout(Some(BOUND)).unwrap();
        remote.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"still sending");

        // A second connection whose receive stream is retained idle (its pump parked in an
        // adapter read) after the socket was dropped.
        let socket = connect(&mut store, peer, &mut tasks).await;
        let (mut idle_peer, _) = listener.accept().unwrap();
        let (idle, _) = receive(&mut store, socket).await;
        drop_handle(&mut store, socket).await;
        idle_peer.write_all(b"y").unwrap();
        assert_eq!(read_bytes(&mut store, idle, 1, &mut tasks).await, b"y");
        retire(&mut store, &mut tasks).await.unwrap();
        // A later callback drops the idle stream; its retire waits for the close sequence.
        drop_handle(&mut store, idle).await;
        assert_eq!(
            closing_pumps(&store),
            1,
            "the closing pump is counted from the DROP"
        );
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(closing_pumps(&store), 0);
        assert!(
            closed_by_host(&mut idle_peer),
            "the idle stream's fd closed"
        );
    });
}

/// A stream closed while its pump waits on backpressure, and another stream opened in the
/// same callback, both behave: the new stream loses nothing and the old pump ends.
#[test]
fn a_reopen_in_the_same_callback_never_reaches_the_closed_stream() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let first = connect(&mut store, peer, &mut tasks).await;
        let (mut first_peer, _) = listener.accept().unwrap();
        let (a, _) = receive(&mut store, first).await;
        // Fill A's pipe so its pump parks on backpressure.
        first_peer.write_all(&vec![7; 3 * PIPE_BYTES]).unwrap();
        let chunk = read(&mut store, a, 16, &mut tasks).await;
        assert_eq!(chunk.bytes[0], 0);
        let second = connect(&mut store, peer, &mut tasks).await;
        let (mut second_peer, _) = listener.accept().unwrap();
        // Same callback, no turn in between: DROP A, open D.
        drop_handle(&mut store, a).await;
        let (d, _) = receive(&mut store, second).await;
        second_peer.write_all(b"new stream data").unwrap();
        assert_eq!(
            read_bytes(&mut store, d, 15, &mut tasks).await,
            b"new stream data"
        );
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(
            closing_pumps(&store),
            0,
            "the stale pump ended without reading again"
        );
        second_peer.write_all(b" and more").unwrap();
        assert_eq!(read_bytes(&mut store, d, 9, &mut tasks).await, b" and more");
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

/// A second concurrent read of a busy adapter stream is refused by a trap, never answered
/// with a false end of stream that would destroy the stream.
#[test]
fn a_busy_adapter_read_traps_instead_of_ending_the_stream() {
    let mut store = sockets(NetworkGrant::default());
    runtime().block_on(async {
        let exports = adapter::test_exports(&mut store).await.unwrap();
        let reader = adapter::idle_stream(&mut store);
        // Opening succeeds on its own turn, so only the second read can cause the failure below.
        let (id,) = store
            .run_concurrent(async move |access| {
                exports.open_bytes.call_concurrent(access, (reader,)).await
            })
            .await
            .unwrap()
            .unwrap();
        let outcome = store
            .run_concurrent(async move |access| {
                let first = exports.read.call_concurrent(access, (id, 8));
                let second = exports.read.call_concurrent(access, (id, 8));
                let (first, second) = futures::future::join(first, second).await;
                (first.map(|(value,)| value), second.map(|(value,)| value))
            })
            .await;
        // The adapter's host-invariant trap (`unreachable`) is the outcome, wherever it surfaces;
        // it poisons the adapter (in production, the revision). No read reports an end of stream.
        let errors = match &outcome {
            Err(error) => vec![format!("{error:#}")],
            Ok((first, second)) => {
                assert!(
                    !matches!(first, Ok(None)) && !matches!(second, Ok(None)),
                    "no false end of stream: {first:?} {second:?}"
                );
                [first, second]
                    .into_iter()
                    .filter_map(|read| read.as_ref().err().map(|error| format!("{error:#}")))
                    .collect()
            }
        };
        assert!(
            errors.iter().any(|error| error.contains("unreachable")),
            "the busy read traps in the adapter: {errors:?}"
        );
    });
}

/// A refused handle issue drops the accepted socket through upstream, closing the client.
#[test]
fn a_refused_accept_issue_closes_the_client() {
    let mut store = sockets(listen_grant());
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let (_socket, accepts, address) = listen(&mut store).await;
        let client = thread::spawn(move || TcpStream::connect(address).unwrap());
        let delivery = read(&mut store, accepts, 1, &mut tasks).await;
        let mut client = client.join().unwrap();
        let accepted = id(&delivery.bytes[5..]);
        // Take the accepted socket back from the table and issue it into a full table.
        let state = store.data_mut();
        let Some(Entry::Resource { kind, rep, .. }) =
            state.standard.handles.entries.remove(&accepted)
        else {
            panic!("accepted socket")
        };
        let pipe: Shared<streams::Pipe> = Shared::default();
        pipe.lock()
            .unwrap()
            .release_into(Arc::clone(&state.standard.handles.closing));
        let filler = (0..HANDLE_LIMIT - state.standard.handles.entries.len())
            .map(|_| state.standard.handles.items().unwrap().0)
            .collect::<Vec<_>>();
        std::mem::take(&mut state.standard.handles.issued);
        adapter::issue(state, &pipe, kind, Resource::new_own(rep), None).unwrap();
        assert_eq!(pipe.lock().unwrap().buffered(), 0, "no item was published");
        assert!(closed_by_host(&mut client), "the refused client is closed");
        for id in filler {
            drop_handle(&mut store, id).await;
        }
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

/// Closing a listen stream while its accept is in flight closes every socket the adapter
/// already took from upstream, and publishes none.
#[test]
fn closing_a_listen_stream_mid_accept_closes_taken_clients() {
    let mut store = sockets(listen_grant());
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let (socket, accepts, address) = listen(&mut store).await;
        // The listen stream keeps the listener alive on its own (shared ownership).
        drop_handle(&mut store, socket).await;
        // Start the pump; its accept is in flight with no client yet.
        let pending = admit(
            &mut store,
            &P::op(STREAM_READ).u64(accepts).u32(1),
            &mut tasks,
        );
        start_streams(&mut store).await.unwrap();
        store
            .run_concurrent(async |_| tokio::task::yield_now().await)
            .await
            .unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        // The client is queued; the guest drops the stream in the same step.
        drop(tasks.revoke());
        drop(pending);
        drop_handle(&mut store, accepts).await;
        let entries = store.data().standard.handles.entries.len();
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(closing_pumps(&store), 0);
        assert!(store.data().standard.handles.entries.len() <= entries);
        assert!(
            closed_by_host(&mut client),
            "a taken or queued client is closed"
        );
    });
}

/// Forced order: the guest drops a listen stream while its pump's `accept` is in flight, and
/// the adapter's read completes (a client arrived) before the pump's `close` runs, so `accept`
/// hands the socket back to the pump. The pump's cleanup must close that connection physically
/// before it returns, which is before the pump releases its closing unit. It must never leave the
/// socket in the table until revision teardown. The real pump reaches this order only if Wasmtime
/// runs the completed read before the newly queued `close`; this test forces it.
#[test]
fn an_accept_returned_after_a_drop_closes_its_client_before_the_pump_ends() {
    let mut store = sockets(listen_grant());
    runtime().block_on(async {
        let (_socket, _accepts, address) = listen(&mut store).await;
        let (reader, kind) = adapter::take_unopened_sockets(&mut store).expect("listen stream");
        let exports = adapter::test_exports(&mut store).await.unwrap();
        let (returned, closed) = store
            .run_concurrent(async move |access| {
                let (id,) = exports
                    .open_sockets
                    .call_concurrent(access, (reader,))
                    .await?;
                let mut accept = std::pin::pin!(exports.accept.call_concurrent(access, (id, 1)));
                let waiting = std::future::poll_fn(|context| {
                    std::task::Poll::Ready(accept.as_mut().poll(context).is_pending())
                })
                .await;
                assert!(waiting, "the accept waits for a client");
                let mut client = TcpStream::connect(address).unwrap();
                // The adapter's read completes before `close` is issued: the forced order.
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                }
                let returned = std::cell::Cell::new(None);
                let read = async {
                    let read = accept.await;
                    if let Ok((accepted,)) = &read {
                        returned.set(Some(accepted.as_ref().map(Vec::len)));
                    }
                    read
                };
                let close = exports.close_sockets.call_concurrent(access, (id,));
                adapter::close_mid_accept(access, kind, close, read).await?;
                // No further Store turn: the connection is already closed when the cleanup
                // returns, so the pump's closing unit can only be released after the close.
                wasmtime::Result::<_>::Ok((returned.get(), closed_by_host(&mut client)))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            returned,
            Some(Some(1)),
            "the forced order hands the socket back"
        );
        assert!(closed, "the returned socket's connection is closed");
    });
}

/// A trap inside the adapter (fuel exhaustion during a pump read) fails the turn with a named
/// context, so the revision is quarantined rather than acknowledging a cleanup.
#[test]
fn an_adapter_trap_fails_the_turn_with_context() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let (mut remote, _) = listener.accept().unwrap();
        let (stream, _) = receive(&mut store, socket).await;
        remote.write_all(b"z").unwrap();
        let receive = admit(
            &mut store,
            &P::op(STREAM_READ).u64(stream).u32(1),
            &mut tasks,
        );
        start_streams(&mut store).await.unwrap();
        store.set_fuel(100).unwrap();
        let Err(error) = store
            .run_concurrent(async |_| tokio::time::timeout(BOUND, receive).await)
            .await
        else {
            panic!("the adapter traps")
        };
        // The runtime attributes the quarantine to the adapter: no interpreter frame trapped.
        let error = adapter::attribute(error, None);
        assert!(
            format!("{error:#}").contains("private stream adapter trapped"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("fuel"), "{error:#}");
    });
}

#[test]
fn udp_datagrams_round_trip_with_granted_peers_only() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(BOUND)).unwrap();
    let address = peer.local_addr().unwrap();
    let mut store = sockets(NetworkGrant {
        datagrams: vec![address],
        ..Default::default()
    });
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        // `create` is WIT-synchronous and Rust-async upstream: awaited on the sync path.
        let created = sync(&mut store, &P::tcp("[static]udp-socket.create").u32(0))
            .await
            .unwrap();
        assert_eq!(created[0], 0);
        let socket = id(&created[1..]);
        let sent = call(
            &mut store,
            &P::tcp("[method]udp-socket.send")
                .u64(socket)
                .bytes(b"ping")
                .u8(1)
                .address(address),
            &mut tasks,
        )
        .await
        .unwrap();
        assert_eq!(sent, [0]);
        let mut buffer = [0; 8];
        let (count, from) = peer.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..count], b"ping");
        peer.send_to(b"pong", from).unwrap();
        let received = call(
            &mut store,
            &P::tcp("[method]udp-socket.receive").u64(socket),
            &mut tasks,
        )
        .await
        .unwrap();
        assert_eq!(received[0], 0);
        assert_eq!(&received[5..9], b"pong");
        // An ungranted destination is refused.
        let denied = call(
            &mut store,
            &P::tcp("[method]udp-socket.send")
                .u64(socket)
                .bytes(b"no")
                .u8(1)
                .address("127.0.0.1:9".parse().unwrap()),
            &mut tasks,
        )
        .await
        .unwrap();
        assert_eq!(denied[0], 1);
        assert_eq!(
            u32::from_le_bytes(denied[1..5].try_into().unwrap()),
            ACCESS_DENIED
        );
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

#[test]
fn name_lookup_uses_the_bound_providers_grant_and_the_tracked_resolver() {
    let lookup = |name: &str| P::call(LOOKUP, "resolve-addresses").bytes(name.as_bytes());
    // `net` grants resolution, but the lookup is bound to `dns`, which does not.
    let mut store = sockets(NetworkGrant {
        resolve: true,
        ..Default::default()
    });
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let denied = call(&mut store, &lookup("localhost"), &mut tasks)
            .await
            .unwrap();
        assert_eq!(denied, [1, 0, 0, 0, 0], "access-denied");
        assert_eq!(store.data().transport.store_work.live(), 0);
    });
    let mut store = store_resolving();
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let literal = call(&mut store, &lookup("127.0.0.1"), &mut tasks)
            .await
            .unwrap();
        assert_eq!(literal, [0, 1, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1]);
        let resolved = call(&mut store, &lookup("localhost"), &mut tasks)
            .await
            .unwrap();
        assert_eq!(resolved[0], 0, "localhost resolves");
        let invalid = call(&mut store, &lookup("a b"), &mut tasks).await.unwrap();
        assert_eq!(invalid, [1, 1, 0, 0, 0], "invalid-argument");
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(store.data().transport.store_work.live(), 0);
    });
}

fn store_resolving() -> Store<State> {
    scoped(NetworkGrant::default(), true)
}

/// The embedded adapter is the reviewed prebuilt; `just check-stream-adapter` proves it is the
/// build of its source.
#[test]
fn the_embedded_adapter_is_the_reviewed_build() {
    use sha2::Digest;
    let bytes = include_bytes!("stream_adapter.wasm");
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(bytes)),
        adapter::COMPONENT_SHA256
    );
}

/// A callback blocked in a TCP send to a peer that never reads is cancelled by its caller (the
/// step driver cancels its command, then retires it): retirement settles promptly, the send's
/// adapter writer is dropped so upstream's consumer ends and the send direction shuts down (the
/// peer drains what the kernel already queued, then sees end of stream), no closing pump or Store
/// work remains, and the revision serves the next call.
#[test]
fn a_cancelled_callback_aborts_its_blocked_send() {
    let (listener, peer) = loopback_listener();
    let (go, ready) = std::sync::mpsc::channel::<()>();
    let reader = thread::spawn(move || {
        let (mut remote, _) = listener.accept().unwrap();
        ready.recv().unwrap();
        remote.set_read_timeout(Some(BOUND)).unwrap();
        let mut drained = 0usize;
        let mut buffer = vec![0; 1 << 20];
        loop {
            match remote.read(&mut buffer) {
                Ok(0) => return (drained, true),
                Ok(count) => drained += count,
                Err(_) => return (drained, false),
            }
        }
    });
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let writer = id(&sync(&mut store, &P::op(STREAM_NEW).u32(BYTE_STREAM))
            .await
            .unwrap());
        sync(
            &mut store,
            &P::tcp("[method]tcp-socket.send").u64(socket).u64(writer),
        )
        .await
        .unwrap();
        // Write until the peer's kernel buffers are full and a write stays pending.
        let chunk = vec![b's'; PIPE_BYTES];
        let mut blocked = false;
        for _ in 0..512 {
            let receive = admit(
                &mut store,
                &P::op(STREAM_WRITE).u64(writer).bytes(&chunk),
                &mut tasks,
            );
            start_streams(&mut store).await.unwrap();
            let written = store
                .run_concurrent(async |_| {
                    tokio::time::timeout(Duration::from_millis(300), receive).await
                })
                .await
                .unwrap();
            if written.is_err() {
                blocked = true;
                break;
            }
        }
        assert!(blocked, "the send never blocked on the peer");
        let cancelled = std::time::Instant::now();
        store.data().transport.commands.cancel();
        retire(&mut store, &mut tasks).await.unwrap();
        let settled = cancelled.elapsed();
        assert!(settled < Duration::from_secs(2), "retired in {settled:?}");
        assert_eq!(closing_pumps(&store), 0);
        assert_eq!(store.data().transport.store_work.live(), 0);
        go.send(()).unwrap();
        let (drained, ended) = reader.join().unwrap();
        assert!(
            ended,
            "the send direction shut down after {drained} queued bytes"
        );
        // The revision still serves calls.
        store.data().transport.commands.begin(None);
        let created = sync(&mut store, &P::tcp("[static]tcp-socket.create").u32(0))
            .await
            .unwrap();
        assert_eq!(created[0], 0);
        retire(&mut store, &mut tasks).await.unwrap();
    });
}

/// A connected socket gets exactly one prepared send writer; a socket the guest drops without
/// sending has its writer closed when the callback retires, so none accumulate.
#[test]
fn unsent_sockets_release_their_prepared_writers() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let first = connect(&mut store, peer, &mut tasks).await;
        let _ = listener.accept().unwrap();
        let second = connect(&mut store, peer, &mut tasks).await;
        let _ = listener.accept().unwrap();
        assert_eq!(adapter::prepared(store.data_mut()), 2);
        drop_handle(&mut store, first).await;
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(
            adapter::prepared(store.data_mut()),
            1,
            "the dropped socket's writer closed"
        );
        drop_handle(&mut store, second).await;
        retire(&mut store, &mut tasks).await.unwrap();
        assert_eq!(adapter::prepared(store.data_mut()), 0);
    });
}

/// A send writer's preparation never abandons the private adapter call when its caller is
/// aborted (a cancelled callback's connect): the caller's wait is pending after its first poll,
/// because the preparation runs in its own Store task. That task still publishes the writer when
/// the socket lives, or closes it when the socket was dropped meanwhile, and retirement waits for
/// it.
#[test]
fn an_aborted_preparation_publishes_or_closes_its_writer() {
    let (listener, peer) = loopback_listener();
    let mut store = sockets(connect_grant(peer));
    runtime().block_on(async {
        let mut tasks = Tasks::default();
        let socket = connect(&mut store, peer, &mut tasks).await;
        let _remote = listener.accept().unwrap();
        let Some(Entry::Resource { rep, .. }) = store.data().standard.handles.entries.get(&socket)
        else {
            panic!("connected socket")
        };
        let rep = *rep;
        let abort_after_first_poll = async |store: &mut wasmtime::Store<State>| {
            store
                .run_concurrent(async |access| {
                    let mut preparing = std::pin::pin!(adapter::prepare(access, rep));
                    assert!(
                        futures::poll!(preparing.as_mut()).is_pending(),
                        "the caller waits for the preparation's own task"
                    );
                    // Dropped here, as an aborted callback task is.
                })
                .await
                .unwrap();
        };

        // The socket lives: the aborted caller's preparation still publishes its writer.
        abort_after_first_poll(&mut store).await;
        retire(&mut store, &mut tasks).await.unwrap();
        assert!(adapter::ready(store.data_mut(), rep));
        assert_eq!(adapter::discarded(store.data_mut()), 0);
        assert_eq!(closing_pumps(&store), 0);

        // The socket is dropped while the preparation is in flight: its writer is closed, never
        // published under the (reusable) rep.
        abort_after_first_poll(&mut store).await;
        drop_handle(&mut store, socket).await;
        retire(&mut store, &mut tasks).await.unwrap();
        assert!(!adapter::ready(store.data_mut(), rep));
        assert_eq!(adapter::prepared(store.data_mut()), 0);
        assert_eq!(adapter::discarded(store.data_mut()), 1);
        assert_eq!(closing_pumps(&store), 0);
    });
}
