//! A compiled component whose async export returns a `stream<u8>` and writes it afterwards.
#![no_std]
extern crate alloc;
use alloc::vec::Vec;

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({ path: "../tests/wit/streams", world: "producer" });

use wit_bindgen::{StreamReader, StreamResult, StreamWriter};

struct Component;

/// Write `count` chunks; `false` once the reader is gone.
async fn write(writer: &mut StreamWriter<u8>, count: u32, size: u32) -> bool {
    for index in 0..count {
        let chunk: Vec<u8> = (0..size)
            .map(|offset| ((index + offset) % 256) as u8)
            .collect();
        if !writer.write_all(chunk).await.is_empty() {
            return false;
        }
    }
    true
}

impl exports::tembra_test::streams::frames::Guest for Component {
    async fn frames(count: u32, size: u32, mode: u32) -> StreamReader<u8> {
        let (mut writer, reader) = wit_stream::new::<u8>();
        wit_bindgen::spawn_local(async move {
            match mode {
                0 => {
                    write(&mut writer, count, size).await;
                }
                1 => {
                    write(&mut writer, count, size).await;
                    core::arch::wasm32::unreachable();
                }
                2 => {
                    write(&mut writer, count, size).await;
                    drop(writer);
                    core::arch::wasm32::unreachable();
                }
                3 => loop {
                    // A zero-length write waits until the reader can take data or is gone.
                    let (status, _) = writer.write(Vec::new()).await;
                    if matches!(status, StreamResult::Dropped) {
                        break;
                    }
                },
                _ => {
                    // Await a stream this task also writes: it never completes, and the
                    // producer never looks at its own reader.
                    let (_idle, mut never) = wit_stream::new::<u8>();
                    let _ = never.read(Vec::with_capacity(1)).await;
                    drop(writer);
                }
            }
        });
        reader
    }

    fn ping(value: u32) -> u32 {
        value + 1
    }
}

export!(Component);
