//! A compiled Orb importing another Orb's WIT resources: it constructs, borrows, keeps and drops
//! them only through ordinary WIT handles. It also serves incoming HTTP: each request constructs
//! and drops one session.
#![no_std]
extern crate alloc;

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{cell::RefCell, future::Future, task::Poll};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use exports::tembra_test::resources::relay::{KeyedReport, KeyedSession};
use tembra_test::resources::sessions::{self, Session};
use tembra_test::resources::views;
use wasi::http::types::{ErrorCode, Fields, Request, Response, Trailers};

/// One owned session kept across calls of this Store.
struct Kept(RefCell<Option<Session>>);
// SAFETY: a core Wasm instance without threads never shares this value between threads.
unsafe impl Sync for Kept {}
static KEPT: Kept = Kept(RefCell::new(None));

struct Relay;

/// An exported resource owning an imported session; dropping it drops that session.
struct Holder {
    session: Session,
}

impl exports::tembra_test::resources::relay::GuestHolder for Holder {
    fn new(seed: u64) -> Self {
        Self {
            session: Session::new(seed),
        }
    }

    fn total(&self) -> u64 {
        self.session.total()
    }
}

impl exports::tembra_test::resources::relay::Guest for Relay {
    type Holder = Holder;

    fn pair(seed: u64, n: u64) -> Vec<u64> {
        let first = Session::new(seed);
        // Equal arguments, distinct instance.
        let second = Session::new(seed);
        let third = sessions::open(n);
        first.add(n);
        vec![first.total(), second.total(), third.add(1)]
    }

    fn keep(seed: u64) -> u64 {
        let session = Session::new(seed);
        let total = session.total();
        KEPT.0.replace(Some(session));
        total
    }

    fn bump(n: u64) -> u64 {
        KEPT.0
            .borrow()
            .as_ref()
            .map_or(u64::MAX, |session| session.add(n))
    }

    fn discard() {
        KEPT.0.take();
    }

    fn closed() -> u64 {
        sessions::closed()
    }

    async fn paced(seed: u64, n: u64) -> u64 {
        let session = Session::new(seed);
        session.paced_add(n).await
    }

    fn describe() -> String {
        views::describe()
    }

    async fn abandon(seed: u64) -> bool {
        let mut later = core::pin::pin!(sessions::open_later(seed));
        // One poll starts the import; dropping the future afterwards cancels its subtask.
        let first =
            core::future::poll_fn(|context| Poll::Ready(later.as_mut().poll(context))).await;
        first.is_ready()
    }

    async fn abandon_started(seed: u64) -> bool {
        let done = {
            let mut later = core::pin::pin!(sessions::open_later(seed));
            let first =
                core::future::poll_fn(|context| Poll::Ready(later.as_mut().poll(context))).await;
            // The checkpoint: a cooperating provider answers only once `open-later` started.
            sessions::closed();
            first.is_ready()
            // Leaving this block drops the pending import, which cancels its subtask.
        };
        // Show that this call continued past the drop.
        sessions::closed();
        done
    }

    fn keyed(first: (String, String), second: (String, String), seed: u64) -> KeyedReport {
        // Equal keys still open distinct instances; each keeps its own key and total.
        let first = (first.0.as_str(), first.1.as_str());
        let second = (second.0.as_str(), second.1.as_str());
        let opened = [
            sessions::open_keyed(first, seed),
            sessions::open_keyed(first, seed),
            sessions::open_keyed(second, seed),
        ];
        let observed = opened
            .iter()
            .zip(1..)
            .map(|(session, n)| {
                session.add(n);
                KeyedSession {
                    key: session.key(),
                    total: session.total(),
                }
            })
            .collect();
        drop(opened);
        KeyedReport {
            sessions: observed,
            // The drops are released only when this call settles, so none has run yet.
            closed_before_settlement: sessions::closed(),
        }
    }
}

impl exports::wasi::http::handler::Guest for Relay {
    /// Answer with the total of a session built for this request; it is dropped before the
    /// response leaves.
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        drop(request);
        let session = Session::new(7);
        let total = session.add(1);
        drop(session);
        let headers = Fields::from_list(&[("x-total".to_string(), total.to_string().into_bytes())])
            .map_err(|_| ErrorCode::InternalError(None))?;
        let (_, trailers) = wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
        Ok(Response::new(headers, None, trailers).0)
    }
}

export!(Relay);
