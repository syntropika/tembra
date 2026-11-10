//! Private stream adapter. It owns socket streams on the guest side: dropping an output
//! stream it reads is the standard component-model reader drop, so the upstream producer (the
//! socket's read half or listener) closes even while no data arrives; dropping an input stream it
//! writes ends upstream's send consumer even while the peer does not read. The host drives each
//! open stream from exactly one pump and never abandons a call; `close` ends an in-progress
//! `read` or `write` through a component-model future signal. Stream ids are never reused within
//! an instance, and a second concurrent `read` or `write` of one stream traps instead of
//! disturbing the first. `no_std`; it imports only the `wasi:sockets/types` resource type.
#![no_std]
extern crate alloc;

use alloc::{collections::BTreeMap, vec::Vec};
use core::{cell::RefCell, future::poll_fn, pin::pin, task::Poll};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    path: [
        "../../contracts/wit/wasi03/clocks.wit",
        "../../contracts/wit/wasi03/sockets.wit",
        "wit",
    ],
    world: "tembra:stream-adapter/adapter",
    generate_all,
});

use wasi::sockets::types::TcpSocket;
use wit_bindgen::{FutureReader, FutureWriter, StreamReader, StreamResult, StreamWriter};

/// A single-threaded cell; wasm32 components run one thread.
struct Local<T>(RefCell<T>);
// SAFETY: wasm32 components run one thread; the cell is never shared across threads.
unsafe impl<T> Sync for Local<T> {}

/// Live streams per element type. The host opens at most one per guest stream handle, which its
/// own handle table bounds well below this.
const LIVE: usize = 4096;

/// A broken host invariant (a second reader, an exhausted id space, too many live streams) is a
/// trap, never a value the host could mistake for a stream state.
fn violation() -> ! {
    core::arch::wasm32::unreachable()
}

/// One open stream: its reader (absent while the one permitted read holds it), a close request,
/// and the signal that wakes the in-progress read.
struct Slot<T: 'static> {
    reader: Option<StreamReader<T>>,
    closing: bool,
    signal: Option<FutureWriter<()>>,
}

/// Live streams keyed by ids that are never reused within this instance, so a stale caller can
/// never reach a stream opened later.
struct Table<T: 'static> {
    live: BTreeMap<u32, Slot<T>>,
    next: u32,
}

/// Open, read and close over one table, for one stream element type.
macro_rules! slots {
    ($table:ident, $item:ty) => {
        static $table: Local<Table<$item>> = Local(RefCell::new(Table {
            live: BTreeMap::new(),
            next: 0,
        }));

        impl Slot<$item> {
            fn open(data: StreamReader<$item>) -> u32 {
                let mut table = $table.0.borrow_mut();
                if table.live.len() >= LIVE {
                    violation();
                }
                let id = table.next;
                table.next = id.checked_add(1).unwrap_or_else(|| violation());
                table.live.insert(
                    id,
                    Slot {
                        reader: Some(data),
                        closing: false,
                        signal: None,
                    },
                );
                id
            }

            async fn read(id: u32, max: u32) -> Option<Vec<$item>> {
                let (mut reader, closed) = {
                    let mut table = $table.0.borrow_mut();
                    let slot = table.live.get_mut(&id)?;
                    if slot.closing {
                        return None;
                    }
                    // Checked before anything is touched: a second reader must not replace the
                    // in-flight read's signal (which would cancel it) nor see a false end.
                    let Some(reader) = slot.reader.take() else {
                        violation();
                    };
                    let (signal, closed): (FutureWriter<()>, FutureReader<()>) =
                        wit_future::new(|| ());
                    slot.signal = Some(signal);
                    (reader, closed)
                };
                let outcome = {
                    let mut read = pin!(reader.read(Vec::with_capacity(max as usize)));
                    let mut closed = pin!(closed.into_future());
                    poll_fn(|cx| {
                        if closed.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(None);
                        }
                        read.as_mut().poll(cx).map(Some)
                    })
                    .await
                    // Dropping `read` here cancels it if it is still in progress.
                };
                let mut table = $table.0.borrow_mut();
                let slot = table.live.get_mut(&id).unwrap_or_else(|| violation());
                match outcome {
                    Some((StreamResult::Complete(_), items)) if !slot.closing => {
                        slot.reader = Some(reader);
                        slot.signal = None;
                        Some(items)
                    }
                    // Ended, or closed while reading: items already taken from the stream are
                    // dropped here, in the guest (owned sockets close through their drop).
                    _ => {
                        drop(reader);
                        table.live.remove(&id);
                        None
                    }
                }
            }

            async fn close(id: u32) {
                let signal = {
                    let mut table = $table.0.borrow_mut();
                    let Some(slot) = table.live.get_mut(&id) else {
                        return;
                    };
                    if slot.reader.is_some() {
                        table.live.remove(&id);
                        return;
                    }
                    slot.closing = true;
                    slot.signal.take()
                };
                if let Some(signal) = signal {
                    let _ = signal.write(()).await;
                }
            }
        }
    };
}

slots!(BYTES, u8);
slots!(SOCKETS, TcpSocket);

/// One open writer: its writable end (absent while the one permitted write holds it), a close
/// request, and the signal that wakes the in-progress write.
struct Outlet {
    writer: Option<StreamWriter<u8>>,
    closing: bool,
    signal: Option<FutureWriter<()>>,
}

/// Live writers keyed by ids that are never reused within this instance.
struct Outlets {
    live: BTreeMap<u32, Outlet>,
    next: u32,
}

static WRITERS: Local<Outlets> = Local(RefCell::new(Outlets {
    live: BTreeMap::new(),
    next: 0,
}));

impl Outlet {
    fn open() -> (u32, StreamReader<u8>) {
        let (writer, reader) = wit_stream::new::<u8>();
        let mut table = WRITERS.0.borrow_mut();
        if table.live.len() >= LIVE {
            violation();
        }
        let id = table.next;
        table.next = id.checked_add(1).unwrap_or_else(|| violation());
        table.live.insert(
            id,
            Outlet {
                writer: Some(writer),
                closing: false,
                signal: None,
            },
        );
        (id, reader)
    }

    async fn write(id: u32, data: Vec<u8>) -> bool {
        let (mut writer, closed) = {
            let mut table = WRITERS.0.borrow_mut();
            let Some(outlet) = table.live.get_mut(&id) else {
                return false;
            };
            if outlet.closing {
                return false;
            }
            // Checked before anything is touched: a second writer must not replace the
            // in-flight write's signal.
            let Some(writer) = outlet.writer.take() else {
                violation();
            };
            let (signal, closed): (FutureWriter<()>, FutureReader<()>) = wit_future::new(|| ());
            outlet.signal = Some(signal);
            (writer, closed)
        };
        let outcome = {
            let mut write = pin!(writer.write_all(data));
            let mut closed = pin!(closed.into_future());
            poll_fn(|cx| {
                if closed.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                write.as_mut().poll(cx).map(Some)
            })
            .await
            // Dropping `write` here cancels it if it is still in progress.
        };
        let mut table = WRITERS.0.borrow_mut();
        let outlet = table.live.get_mut(&id).unwrap_or_else(|| violation());
        match outcome {
            Some(rest) if !outlet.closing && rest.is_empty() => {
                outlet.writer = Some(writer);
                outlet.signal = None;
                true
            }
            // The reader is gone, or closed while writing: the writable end drops here.
            _ => {
                drop(writer);
                table.live.remove(&id);
                false
            }
        }
    }

    async fn close(id: u32) {
        let signal = {
            let mut table = WRITERS.0.borrow_mut();
            let Some(outlet) = table.live.get_mut(&id) else {
                return;
            };
            if outlet.writer.is_some() {
                table.live.remove(&id);
                return;
            }
            outlet.closing = true;
            outlet.signal.take()
        };
        if let Some(signal) = signal {
            let _ = signal.write(()).await;
        }
    }
}

struct Adapter;

impl exports::tembra::stream_adapter::bytes::Guest for Adapter {
    fn open(data: StreamReader<u8>) -> u32 {
        Slot::<u8>::open(data)
    }
    async fn read(id: u32, max: u32) -> Option<Vec<u8>> {
        Slot::<u8>::read(id, max).await
    }
    async fn close(id: u32) {
        Slot::<u8>::close(id).await
    }
    fn signal(signal: FutureReader<()>) {
        drop(signal);
    }
}

impl exports::tembra::stream_adapter::sockets::Guest for Adapter {
    fn open(sockets: StreamReader<TcpSocket>) -> u32 {
        Slot::<TcpSocket>::open(sockets)
    }
    async fn accept(id: u32, max: u32) -> Option<Vec<TcpSocket>> {
        Slot::<TcpSocket>::read(id, max).await
    }
    async fn close(id: u32) {
        Slot::<TcpSocket>::close(id).await
    }
}

impl exports::tembra::stream_adapter::writes::Guest for Adapter {
    fn open() -> (u32, StreamReader<u8>) {
        Outlet::open()
    }
    async fn write(id: u32, data: Vec<u8>) -> bool {
        Outlet::write(id, data).await
    }
    async fn close(id: u32) {
        Outlet::close(id).await
    }
}

export!(Adapter);
