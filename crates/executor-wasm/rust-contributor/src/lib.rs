//! A compiled Orb contributing to waterfall, series and bail hooks of another Orb.
//!
//! Its packaging selects priority -7 for every callback, so it runs before the source and native
//! participants of the qualification fixtures. The imported hook interfaces are type anchors only.
//! It is `no_std`, like the probe, so its component imports exactly its WIT world.
#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};
use exports::example::hooks::{audit, choice, guard, reporter};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Sum of audited inputs observed by this contributor.
static AUDIT: AtomicI32 = AtomicI32::new(0);
/// Number of choice calls that reached this contributor.
static CHOICES: AtomicI32 = AtomicI32::new(0);

struct Contributor;

impl guard::Guest for Contributor {
    /// Raise the value and leave the decision to later participants.
    fn apply(input: guard::Admission) -> guard::Admission {
        guard::Admission {
            value: input.value + 20,
            blocked: input.blocked,
        }
    }
}

impl audit::Guest for Contributor {
    fn apply(input: i32) {
        AUDIT.fetch_add(input, Ordering::Relaxed);
    }
}

impl choice::Guest for Contributor {
    /// Decide only input 1; `none` lets the next participant decide.
    fn apply(input: i32) -> Option<i32> {
        CHOICES.fetch_add(1, Ordering::Relaxed);
        (input == 1).then_some(42)
    }
}

impl reporter::Guest for Contributor {
    fn read() -> reporter::Counts {
        reporter::Counts {
            audit: AUDIT.load(Ordering::Relaxed),
            choices: CHOICES.load(Ordering::Relaxed),
        }
    }
}

export!(Contributor);
