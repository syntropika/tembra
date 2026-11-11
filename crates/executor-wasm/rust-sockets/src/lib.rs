//! Compiled `wasi:sockets` fixture: TCP idle drop, UDP, listen/accept, name lookup, retained
//! streams, and pending connects, reads, accepts and sends the host can cancel.
#![no_std]
extern crate alloc;
use alloc::{format, string::String, vec, vec::Vec};
use core::cell::RefCell;
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
use wasi::sockets::types::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket, UdpSocket,
};
use wit_bindgen::{FutureReader, StreamReader, StreamResult};

/// A single-threaded cell; wasm32 components run one thread.
struct Local<T>(RefCell<T>);
// SAFETY: wasm32 components run one thread; the cell is never shared across threads.
unsafe impl<T> Sync for Local<T> {}

/// Retained across calls: a connected socket with its unread receive stream and completion.
type Received = (
    TcpSocket,
    StreamReader<u8>,
    FutureReader<Result<(), wasi::sockets::types::ErrorCode>>,
);
static RECEIVE: Local<Option<Received>> = Local(RefCell::new(None));
/// Retained across calls: a listening socket with its unread accept stream.
static LISTEN: Local<Option<(TcpSocket, StreamReader<TcpSocket>)>> = Local(RefCell::new(None));
/// What the initializer opened: a socket and the `receive` results of it, unconnected.
static INITIALIZED: Local<Option<Result<Received, String>>> = Local(RefCell::new(None));

/// Runs while the component is instantiated (wit-component calls `_initialize` then): a socket
/// stream opened before any export runs, without a connect.
#[unsafe(no_mangle)]
pub extern "C" fn _initialize() {
    let opened = TcpSocket::create(IpAddressFamily::Ipv4)
        .map(|socket| {
            let (stream, complete) = socket.receive();
            (socket, stream, complete)
        })
        .map_err(|e| format!("create: {e:?}"));
    *INITIALIZED.0.borrow_mut() = Some(opened);
}

async fn connected(port: u16) -> Result<TcpSocket, String> {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
    socket
        .connect(loopback(port))
        .await
        .map_err(|e| format!("connect: {e:?}"))?;
    Ok(socket)
}

fn listening(port: u16) -> Result<(TcpSocket, StreamReader<TcpSocket>), String> {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
    socket
        .bind(loopback(port))
        .map_err(|e| format!("bind: {e:?}"))?;
    let accepts = socket.listen().map_err(|e| format!("listen: {e:?}"))?;
    Ok((socket, accepts))
}

fn loopback(port: u16) -> IpSocketAddress {
    IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port,
        address: (127, 0, 0, 1),
    })
}

fn checked<T: core::fmt::Debug + PartialEq>(
    actual: Result<T, ErrorCode>,
    expected: T,
    label: &str,
) -> Result<(), String> {
    match actual {
        Ok(value) if value == expected => Ok(()),
        other => Err(format!("{label}: expected {expected:?}, got {other:?}")),
    }
}

fn invalid<T: core::fmt::Debug>(actual: Result<T, ErrorCode>, label: &str) -> Result<(), String> {
    match actual {
        Err(ErrorCode::InvalidArgument) => Ok(()),
        other => Err(format!("{label}: expected InvalidArgument, got {other:?}")),
    }
}

fn checked_address(
    actual: Result<IpSocketAddress, ErrorCode>,
    port: u16,
    label: &str,
) -> Result<(), String> {
    match actual {
        Ok(IpSocketAddress::Ipv4(value))
            if value.port == port && value.address == (127, 0, 0, 1) =>
        {
            Ok(())
        }
        other => Err(format!("{label}: unexpected address {other:?}")),
    }
}

struct Probe;
impl exports::example::sockets::probe::Guest for Probe {
    fn socket_options() -> Result<Vec<String>, String> {
        let tcp = TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("{e:?}"))?;
        let udp = UdpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("{e:?}"))?;
        let mut report = Vec::new();
        // Every roundtrip crosses generated WIT bindings; values above JS's safe integer
        // range deliberately verify that the duration and buffer lanes remain exact u64.
        macro_rules! roundtrip {
            ($socket:ident, $set:ident, $get:ident, $value:expr) => {{
                checked($socket.$set($value), (), stringify!($set))?;
                checked($socket.$get(), $value, stringify!($get))?;
                report.push(String::from(concat!(
                    stringify!($socket),
                    ".",
                    stringify!($get)
                )));
            }};
        }
        macro_rules! zero {
            ($socket:ident, $set:ident, $value:expr) => {{
                invalid($socket.$set($value), stringify!($set))?;
                report.push(String::from(concat!(
                    stringify!($socket),
                    ".",
                    stringify!($set),
                    ".zero"
                )));
            }};
        }
        roundtrip!(tcp, set_keep_alive_enabled, get_keep_alive_enabled, true);
        roundtrip!(
            tcp,
            set_keep_alive_idle_time,
            get_keep_alive_idle_time,
            u64::MAX - 2
        );
        roundtrip!(
            tcp,
            set_keep_alive_interval,
            get_keep_alive_interval,
            (1u64 << 53) + 1
        );
        roundtrip!(tcp, set_keep_alive_count, get_keep_alive_count, 13);
        roundtrip!(tcp, set_hop_limit, get_hop_limit, 37);
        roundtrip!(
            tcp,
            set_receive_buffer_size,
            get_receive_buffer_size,
            u64::MAX - 4
        );
        roundtrip!(
            tcp,
            set_send_buffer_size,
            get_send_buffer_size,
            (1u64 << 53) + 3
        );
        roundtrip!(udp, set_unicast_hop_limit, get_unicast_hop_limit, 41);
        roundtrip!(
            udp,
            set_receive_buffer_size,
            get_receive_buffer_size,
            u64::MAX - 6
        );
        roundtrip!(
            udp,
            set_send_buffer_size,
            get_send_buffer_size,
            (1u64 << 53) + 5
        );
        zero!(tcp, set_keep_alive_idle_time, 0);
        zero!(tcp, set_keep_alive_interval, 0);
        zero!(tcp, set_keep_alive_count, 0);
        zero!(tcp, set_hop_limit, 0);
        zero!(tcp, set_receive_buffer_size, 0);
        zero!(tcp, set_send_buffer_size, 0);
        zero!(tcp, set_listen_backlog_size, 0);
        zero!(udp, set_unicast_hop_limit, 0);
        zero!(udp, set_receive_buffer_size, 0);
        zero!(udp, set_send_buffer_size, 0);
        Ok(report)
    }

    async fn socket_addresses(port: u16, listen_port: u16) -> Result<Vec<String>, String> {
        let socket = connected(port).await?;
        if !matches!(socket.get_address_family(), IpAddressFamily::Ipv4)
            || socket.get_is_listening()
        {
            return Err(String::from("connected socket family/state"));
        }
        checked_address(socket.get_remote_address(), port, "TCP remote")?;
        socket
            .get_local_address()
            .map_err(|e| format!("local: {e:?}"))?;
        let server = TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("{e:?}"))?;
        checked(
            server.set_listen_backlog_size(u64::MAX),
            (),
            "backlog before listen",
        )?;
        checked(server.bind(loopback(listen_port)), (), "listener bind")?;
        let accepted = server.listen().map_err(|e| format!("listen: {e:?}"))?;
        if !server.get_is_listening() {
            return Err(String::from("listener state"));
        }
        checked_address(server.get_local_address(), listen_port, "listener local")?;
        checked(
            server.set_listen_backlog_size(9),
            (),
            "backlog after listen",
        )?;
        drop(accepted);
        let v6 = TcpSocket::create(IpAddressFamily::Ipv6).map_err(|e| format!("{e:?}"))?;
        if !matches!(v6.get_address_family(), IpAddressFamily::Ipv6) {
            return Err(String::from("IPv6 family"));
        }
        invalid(v6.bind(loopback(port)), "wrong TCP family")?;
        Ok(vec![
            String::from("tcp.addresses"),
            String::from("tcp.listening"),
            String::from("tcp.backlog"),
            String::from("tcp.families"),
        ])
    }

    async fn udp_connected(port: u16) -> Result<Vec<String>, String> {
        let socket = UdpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("{e:?}"))?;
        checked(socket.bind(loopback(0)), (), "UDP bind")?;
        socket
            .get_local_address()
            .map_err(|e| format!("UDP local: {e:?}"))?;
        checked(socket.connect(loopback(port)), (), "UDP connect")?;
        checked_address(socket.get_remote_address(), port, "UDP remote")?;
        if !matches!(socket.get_address_family(), IpAddressFamily::Ipv4) {
            return Err(String::from("UDP IPv4 family"));
        }
        checked(
            socket.send(vec![b'x'], None).await,
            (),
            "connected UDP send",
        )?;
        let (bytes, remote) = socket
            .receive()
            .await
            .map_err(|e| format!("UDP receive: {e:?}"))?;
        checked_address(Ok(remote), port, "UDP received peer")?;
        if bytes != b"echo:x" {
            return Err(format!("connected UDP receive: {bytes:?}"));
        }
        invalid(
            socket.send(vec![0], Some(loopback(port + 1))).await,
            "different connected peer",
        )?;
        checked(socket.disconnect(), (), "UDP disconnect")?;
        if !matches!(socket.get_remote_address(), Err(ErrorCode::InvalidState)) {
            return Err(String::from("UDP remote after disconnect"));
        }
        invalid(
            socket.send(vec![0], None).await,
            "missing disconnected peer",
        )?;
        let v6 = UdpSocket::create(IpAddressFamily::Ipv6).map_err(|e| format!("{e:?}"))?;
        if !matches!(v6.get_address_family(), IpAddressFamily::Ipv6) {
            return Err(String::from("UDP IPv6 family"));
        }
        invalid(v6.bind(loopback(port)), "wrong UDP family")?;
        Ok(vec![
            String::from("udp.bind"),
            String::from("udp.addresses"),
            String::from("udp.connect"),
            String::from("udp.connected-io"),
            String::from("udp.disconnect"),
            String::from("udp.families"),
        ])
    }

    async fn read_drop(port: u16) -> Result<String, String> {
        let socket =
            TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
        socket
            .connect(loopback(port))
            .await
            .map_err(|e| format!("connect: {e:?}"))?;
        let (mut stream, complete) = socket.receive();
        let (_, bytes) = stream.read(Vec::with_capacity(5)).await;
        drop(stream);
        drop(complete);
        drop(socket);
        String::from_utf8(bytes).map_err(|_| String::from("invalid utf8"))
    }

    async fn udp_echo(port: u16, message: String) -> Result<String, String> {
        let socket =
            UdpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
        socket
            .send(message.into_bytes(), Some(loopback(port)))
            .await
            .map_err(|e| format!("send: {e:?}"))?;
        let (bytes, _) = socket
            .receive()
            .await
            .map_err(|e| format!("receive: {e:?}"))?;
        String::from_utf8(bytes).map_err(|_| String::from("invalid utf8"))
    }

    async fn serve(port: u16, clients: u32) -> Result<Vec<String>, String> {
        let socket =
            TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
        socket
            .bind(loopback(port))
            .map_err(|e| format!("bind: {e:?}"))?;
        let mut accepts = socket.listen().map_err(|e| format!("listen: {e:?}"))?;
        let mut texts = Vec::new();
        for _ in 0..clients {
            let (_, mut accepted) = accepts.read(Vec::with_capacity(1)).await;
            let client = accepted
                .pop()
                .ok_or_else(|| String::from("listen stream ended"))?;
            let (mut data, done) = client.receive();
            let mut bytes = Vec::new();
            loop {
                let (status, chunk) = data.read(Vec::with_capacity(64)).await;
                bytes.extend(chunk);
                if !matches!(status, StreamResult::Complete(_)) {
                    break;
                }
            }
            drop(data);
            drop(done);
            drop(client);
            texts.push(String::from_utf8(bytes).map_err(|_| String::from("invalid utf8"))?);
        }
        drop(accepts);
        drop(socket);
        Ok(texts)
    }

    async fn resolve(name: String) -> Result<Vec<String>, String> {
        let addresses = wasi::sockets::ip_name_lookup::resolve_addresses(name)
            .await
            .map_err(|e| format!("{e:?}"))?;
        Ok(addresses
            .into_iter()
            .map(|address| match address {
                IpAddress::Ipv4((a, b, c, d)) => format!("{a}.{b}.{c}.{d}"),
                IpAddress::Ipv6(parts) => format!("{parts:?}"),
            })
            .collect())
    }

    async fn connect(port: u16) -> Result<String, String> {
        let socket =
            TcpSocket::create(IpAddressFamily::Ipv4).map_err(|e| format!("create: {e:?}"))?;
        socket
            .connect(loopback(port))
            .await
            .map_err(|e| format!("connect: {e:?}"))?;
        Ok(String::from("connected"))
    }

    async fn read_pending(port: u16) -> Result<String, String> {
        let socket = connected(port).await?;
        let (mut stream, _complete) = socket.receive();
        let (status, bytes) = stream.read(Vec::with_capacity(64)).await;
        match status {
            StreamResult::Complete(_) => {
                String::from_utf8(bytes).map_err(|_| String::from("invalid utf8"))
            }
            _ => Ok(String::from("end")),
        }
    }

    async fn accept_pending(port: u16) -> Result<String, String> {
        let (_socket, mut accepts) = listening(port)?;
        let (_, accepted) = accepts.read(Vec::with_capacity(1)).await;
        Ok(String::from(if accepted.is_empty() {
            "end"
        } else {
            "accepted"
        }))
    }

    async fn retain_receive(port: u16) -> Result<(), String> {
        let socket = connected(port).await?;
        let (stream, complete) = socket.receive();
        *RECEIVE.0.borrow_mut() = Some((socket, stream, complete));
        Ok(())
    }

    async fn read_retained(count: u32) -> Result<String, String> {
        let Some((socket, mut stream, complete)) = RECEIVE.0.borrow_mut().take() else {
            return Err(String::from("nothing retained"));
        };
        let mut bytes = Vec::new();
        while bytes.len() < count as usize {
            let (status, chunk) = stream.read(Vec::with_capacity(count as usize)).await;
            bytes.extend(chunk);
            if !matches!(status, StreamResult::Complete(_)) {
                break;
            }
        }
        *RECEIVE.0.borrow_mut() = Some((socket, stream, complete));
        String::from_utf8(bytes).map_err(|_| String::from("invalid utf8"))
    }

    async fn retain_listen(port: u16) -> Result<(), String> {
        *LISTEN.0.borrow_mut() = Some(listening(port)?);
        Ok(())
    }

    async fn retained_completion(drop_stream: bool) -> Result<String, String> {
        let Some((socket, stream, complete)) = RECEIVE.0.borrow_mut().take() else {
            return Err(String::from("nothing retained"));
        };
        // The stream is dropped before awaiting (an explicit DROP), or held until after it.
        let held = if drop_stream {
            drop(stream);
            None
        } else {
            Some(stream)
        };
        let result = complete.await;
        drop(held);
        drop(socket);
        Ok(match result {
            Ok(()) => String::from("ok"),
            Err(error) => format!("{error:?}"),
        })
    }

    async fn initialized() -> Result<String, String> {
        let (socket, mut stream, complete) = INITIALIZED
            .0
            .borrow_mut()
            .take()
            .ok_or_else(|| String::from("the initializer did not run"))??;
        let (status, bytes) = stream.read(Vec::with_capacity(16)).await;
        let completion = complete.await;
        drop(stream);
        drop(socket);
        Ok(format!(
            "read={} status={} completion={completion:?}",
            bytes.len(),
            match status {
                StreamResult::Complete(_) => "complete",
                StreamResult::Dropped => "end",
                StreamResult::Cancelled => "cancelled",
            }
        ))
    }

    fn drop_retained() {
        drop(RECEIVE.0.borrow_mut().take());
        drop(LISTEN.0.borrow_mut().take());
    }

    async fn send_blocked(port: u16, size: u32) -> Result<String, String> {
        let socket = connected(port).await?;
        let (mut writer, reader) = wit_stream::new::<u8>();
        let sent = socket.send(reader);
        let rest = writer.write_all(vec![b's'; size as usize]).await;
        drop(writer);
        if !rest.is_empty() {
            return Ok(format!("unsent {}: {:?}", rest.len(), sent.await));
        }
        sent.await.map_err(|e| format!("send: {e:?}"))?;
        Ok(String::from("sent"))
    }
}
export!(Probe);
