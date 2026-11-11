//! A compiled Orb exporting WIT resources: every construction is a fresh guest instance, and
//! each instance's destructor runs once when its last owner releases it.
//!
//! It is `no_std`, like the other fixtures, so its component imports exactly its WIT world.
#![no_std]
extern crate alloc;

use alloc::{string::String, vec};
use core::cell::Cell;

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use exports::tembra_test::resources::sessions::{
    Bundle, Counter as CounterHandle, Guest, GuestCounter, GuestFragile, GuestSession, Selection,
    Session,
};
use tembra_test::resources::pace;

/// A component instance is single-threaded; this counter is its own Store's state.
struct Closed(Cell<u64>);
// SAFETY: a core Wasm instance without threads never shares this value between threads.
unsafe impl Sync for Closed {}
static CLOSED: Closed = Closed(Cell::new(0));

struct Provider;

/// One session's own state; nothing here is shared with any other instance.
struct Instance {
    key: (String, String),
    total: Cell<u64>,
}

impl Instance {
    fn keyed(key: (String, String), seed: u64) -> Self {
        Self {
            key,
            total: Cell::new(seed),
        }
    }
}

impl GuestSession for Instance {
    fn new(seed: u64) -> Self {
        Self::keyed((String::new(), String::new()), seed)
    }

    fn add(&self, n: u64) -> u64 {
        let total = self.total.get() + n;
        self.total.set(total);
        total
    }

    fn total(&self) -> u64 {
        self.total.get()
    }

    /// The borrow of `self` spans a real suspension: the host decides when `wait` resolves.
    async fn paced_add(&self, n: u64) -> u64 {
        let answer = pace::wait(n).await;
        self.add(answer)
    }

    fn key(&self) -> (String, String) {
        self.key.clone()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        CLOSED.0.set(CLOSED.0.get() + 1);
    }
}

struct Counter {
    hits: Cell<u64>,
}

impl GuestCounter for Counter {
    fn new() -> Self {
        Self { hits: Cell::new(0) }
    }

    fn hits(&self) -> u64 {
        self.hits.set(self.hits.get() + 1);
        self.hits.get()
    }
}

/// A resource whose destructor attempts an Orb import.
struct Fragile;

impl GuestFragile for Fragile {
    fn new() -> Self {
        Self
    }
}

impl Drop for Fragile {
    fn drop(&mut self) {
        pace::note(1);
    }
}

impl Guest for Provider {
    type Session = Instance;
    type Counter = Counter;
    type Fragile = Fragile;

    fn open(seed: u64) -> Session {
        Session::new(Instance::new(seed))
    }

    async fn open_later(seed: u64) -> Session {
        let seed = pace::wait(seed).await;
        Session::new(Instance::new(seed))
    }

    fn open_keyed(key: (String, String), seed: u64) -> Session {
        Session::new(Instance::keyed(key, seed))
    }

    fn open_padded(seed: u64, padding: u32) -> Bundle {
        let mut bundle = Self::open_bundle(seed, false).unwrap();
        bundle.label = "x".repeat(padding as usize);
        bundle
    }

    fn open_bundle(seed: u64, fail: bool) -> Result<Bundle, String> {
        if fail {
            return Err("refused".into());
        }
        Ok(Bundle {
            primary: Session::new(Instance::new(seed)),
            optional: (seed != 0).then(|| Session::new(Instance::new(seed + 1))),
            extras: if seed == 0 {
                vec![]
            } else {
                vec![
                    Session::new(Instance::new(seed + 2)),
                    Session::new(Instance::new(seed + 3)),
                ]
            },
            count: CounterHandle::new(Counter::new()),
            seed,
            label: "bundle".into(),
        })
    }

    fn open_pair(seed: u64) -> (Session, u64) {
        (Session::new(Instance::new(seed)), seed)
    }

    fn choose(seed: u64) -> Selection {
        if seed == 0 {
            Selection::Empty
        } else {
            Selection::Active(Session::new(Instance::new(seed + 1)))
        }
    }

    fn take(value: Session) -> u64 {
        value.get::<Instance>().total()
    }

    async fn take_later(value: Session) -> u64 {
        pace::wait(0).await;
        value.get::<Instance>().total()
    }

    fn take_bundle(value: Bundle) -> alloc::vec::Vec<u64> {
        let mut totals = vec![value.primary.get::<Instance>().total()];
        if let Some(optional) = &value.optional {
            totals.push(optional.get::<Instance>().total());
        }
        totals.extend(
            value
                .extras
                .iter()
                .map(|session| session.get::<Instance>().total()),
        );
        totals.push(value.count.get::<Counter>().hits());
        totals.push(value.seed);
        totals
    }

    fn closed() -> u64 {
        CLOSED.0.get()
    }

    fn label() -> String {
        "sessions".into()
    }
}

impl exports::tembra_test::resources::views::Guest for Provider {
    fn describe() -> String {
        "views".into()
    }
}

export!(Provider);
