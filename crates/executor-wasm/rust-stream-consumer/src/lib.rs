//! A compiled component reading `stream<u8>` results of an imported Orb in its own Store.
#![no_std]
extern crate alloc;
use alloc::{boxed::Box, vec::Vec};
use core::{cell::RefCell, future::Future, pin::Pin, task::Poll};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({ path: "../tests/wit/streams", world: "consumer" });

use tembra_test::streams::frames;
use wit_bindgen::{StreamReader, StreamResult};

/// A single-threaded cell; wasm32 components run one thread.
struct Local<T>(RefCell<T>);
// SAFETY: wasm32 components run one thread; the cell is never shared across threads.
unsafe impl<T> Sync for Local<T> {}

/// A stream kept across calls by `retain`.
static RETAINED: Local<Option<StreamReader<u8>>> = Local(RefCell::new(None));

/// Read at most `take` bytes in 4 KiB reads; the bytes read and an order-sensitive checksum.
async fn drain(reader: &mut StreamReader<u8>, take: u64) -> (u64, u64) {
    let (mut bytes, mut checksum) = (0_u64, 0_u64);
    while bytes < take {
        let want = core::cmp::min(4096, take - bytes) as usize;
        let (status, chunk) = reader.read(Vec::with_capacity(want)).await;
        for byte in &chunk {
            checksum = checksum.wrapping_mul(31).wrapping_add(u64::from(*byte));
        }
        bytes += chunk.len() as u64;
        if matches!(status, StreamResult::Dropped) {
            break;
        }
    }
    (bytes, checksum)
}

struct Component;

impl exports::tembra_test::streams::digest::Guest for Component {
    async fn sum(count: u32, size: u32, mode: u32, take: u64) -> (u64, u64) {
        let mut reader = frames::frames(count, size, mode).await;
        drain(&mut reader, take).await
    }

    async fn abandon(count: u32, size: u32) -> bool {
        let mut later = core::pin::pin!(frames::frames(count, size, 0));
        // One poll starts the import; dropping the future afterwards cancels its subtask.
        let first =
            core::future::poll_fn(|context| Poll::Ready(later.as_mut().poll(context))).await;
        first.is_ready()
    }

    async fn retain(count: u32, size: u32) -> u32 {
        let reader = frames::frames(count, size, 0).await;
        *RETAINED.0.borrow_mut() = Some(reader);
        0
    }

    async fn retained() -> u64 {
        let Some(mut reader) = RETAINED.0.borrow_mut().take() else {
            return u64::MAX;
        };
        drain(&mut reader, u64::MAX).await.0
    }

    async fn trap(count: u32, size: u32, take: u64) -> u32 {
        let mut reader = frames::frames(count, size, 0).await;
        drain(&mut reader, take).await;
        core::arch::wasm32::unreachable()
    }

    async fn many(n: u32, count: u32, size: u32) -> u64 {
        // Every import is started before any stream is read.
        let mut opening: Vec<Pin<Box<dyn Future<Output = StreamReader<u8>>>>> = (0..n)
            .map(|_| Box::pin(frames::frames(count, size, 0)) as Pin<Box<dyn Future<Output = _>>>)
            .collect();
        let mut readers: Vec<Option<StreamReader<u8>>> = (0..n).map(|_| None).collect();
        core::future::poll_fn(|context| {
            let mut pending = false;
            for (future, reader) in opening.iter_mut().zip(readers.iter_mut()) {
                if reader.is_none() {
                    match future.as_mut().poll(context) {
                        Poll::Ready(opened) => *reader = Some(opened),
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        let mut total = 0;
        for reader in readers.iter_mut().flatten() {
            total += drain(reader, u64::MAX).await.0;
        }
        total
    }

    async fn drop_then_ping(count: u32, size: u32) -> u32 {
        let reader = frames::frames(count, size, 0).await;
        // The callback stays alive while the imported call below waits.
        drop(reader);
        frames::ping(1)
    }

    fn ping(value: u32) -> u32 {
        value + 1
    }
}

export!(Component);
