//! A compiled Orb contributing to another Orb's hook. Its counts live in statics for as long as
//! the Orb stays installed, and a service of its own reports them: a contribution runs in the
//! Orb's root, which owns no kernel state (see examples/rust-orb-state for a service that does).
#![cfg_attr(target_arch = "wasm32", no_std)]

#[tembra_orb_macro::orb(package = "example:filter@1.0.0", wit = "../wit")]
mod filter {
    use core::sync::atomic::{AtomicU32, Ordering};

    /// Admissions this contributor has seen, and how many it blocked.
    static SEEN: AtomicU32 = AtomicU32::new(0);
    static BLOCKED: AtomicU32 = AtomicU32::new(0);

    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:admission/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        SEEN.fetch_add(1, Ordering::Relaxed);
        if input.value < 0 {
            BLOCKED.fetch_add(1, Ordering::Relaxed);
            return guard::Admission {
                value: input.value,
                blocked: true,
            };
        }
        input
    }

    /// What this contributor observed.
    pub struct Counts {
        pub seen: u32,
        pub blocked: u32,
    }

    /// The contributor's own service.
    #[service]
    pub struct Report;

    impl Report {
        /// Admissions seen and blocked so far.
        pub fn read() -> Counts {
            Counts {
                seen: SEEN.load(Ordering::Relaxed),
                blocked: BLOCKED.load(Ordering::Relaxed),
            }
        }
    }
}
