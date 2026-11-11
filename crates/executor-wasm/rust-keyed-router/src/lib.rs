//! A compiled Orb selecting several kernel objects through typed WIT references.
#![no_std]
extern crate alloc;

use alloc::{format, string::String, vec};
use core::{cell::RefCell, future::Future, task::Poll};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
use example::keyed::counter_ref::{self, Counter};

struct Kept(RefCell<Option<Counter>>);
// SAFETY: this core Wasm fixture has no threads.
unsafe impl Sync for Kept {}
static KEPT: Kept = Kept(RefCell::new(None));

/// Poll both acquisitions before either is ready, without a second executor.
async fn together<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (core::pin::pin!(a), core::pin::pin!(b));
    let (mut left, mut right) = (None, None);
    core::future::poll_fn(|cx| {
        if left.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(cx)
        {
            left = Some(value);
        }
        if right.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            right = Some(value);
        }
        if left.is_some() && right.is_some() {
            Poll::Ready((left.take().unwrap(), right.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

struct Router;
impl exports::example::keyed::compiled_router::Guest for Router {
    async fn route(key: String, other: String, amount: u32) -> String {
        let first_key = vec![key];
        let other_key = vec![other];
        let (first, second) = together(
            counter_ref::get(first_key.clone()),
            counter_ref::get(other_key),
        )
        .await;
        let (first, second) = match (first, second) {
            (Ok(first), Ok(second)) => (first, second),
            (Err(error), _) | (_, Err(error)) => return format!("refused: {error}"),
        };
        let again = match counter_ref::get(first_key.clone()).await {
            Ok(reference) => reference,
            Err(error) => return format!("refused: {error}"),
        };
        format!(
            "{},{},{} {} {}",
            first.add(amount),
            again.add(amount),
            second.add(amount),
            again.who(),
            second.who()
        )
    }

    async fn probe(name: String) -> String {
        if name == "abandon-wait" {
            {
                let mut pending = core::pin::pin!(counter_ref::get(vec![String::from("settles")]));
                core::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_ready())).await;
            }
            // Keep this callback open beyond the abandoned acquisition's readiness.
            return match counter_ref::get(vec![String::from("barrier")]).await {
                Ok(reference) => reference.who(),
                Err(error) => format!("refused: {error}"),
            };
        }
        if name == "oversized" {
            // Escaping expands this key beyond a host packet budget of 8 KiB. It must take
            // the acquisition's WIT error lane, without affecting other objects in the Store.
            return match counter_ref::get(vec!["\0".repeat(2048)]).await {
                Ok(_) => String::from("accepted"),
                Err(error) => format!("refused: {error}"),
            };
        }
        if name == "abandon" {
            let key = vec![String::from("slow")];
            let mut pending = core::pin::pin!(counter_ref::get(key));
            let ready =
                core::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx).is_ready())).await;
            return format!("ready: {ready}");
        }
        if name == "retained" {
            // Allocate another slot first: stale handles must never select this fresh claim.
            let _fresh = counter_ref::get(vec![String::from("fresh")]).await;
            return KEPT.0.borrow().as_ref().unwrap().who();
        }
        match counter_ref::get(vec![name.clone()]).await {
            Ok(reference) => {
                let name = reference.who();
                if name.starts_with("keep@") {
                    KEPT.0.replace(Some(reference));
                }
                name
            }
            Err(error) => format!("refused: {error}"),
        }
    }
}

export!(Router);
