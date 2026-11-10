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
    IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket, UdpSocket,
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

struct Probe;
impl exports::example::sockets::probe::Guest for Probe {
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
