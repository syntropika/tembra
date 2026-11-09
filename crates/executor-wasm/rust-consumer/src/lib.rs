//! A compiled Orb that depends on two other Orbs through ordinary WIT imports.
//!
//! It is `no_std`, like the probe, so its component imports exactly its WIT world.
#![no_std]
extern crate alloc;

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use example::pipeline::{arithmetic, delayed};

struct Pipeline;

impl exports::example::pipeline::pipeline::Guest for Pipeline {
    /// Block only this guest while the selected provider answers each synchronous import.
    fn quadruple(value: i32) -> i32 {
        arithmetic::double(arithmetic::double(value))
    }

    /// Await three sequential P3 async imports from an async export.
    async fn octuple(value: i32) -> i32 {
        let value = delayed::double(value).await;
        let value = delayed::double(value).await;
        delayed::double(value).await
    }
}

export!(Pipeline);
