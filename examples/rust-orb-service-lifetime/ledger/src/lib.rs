//! A compiled Orb with three services of one shape and three lifetimes. Each keeps a balance in
//! kernel-managed state, so what survives the release of a reference shows what the lifetime
//! means: a pinned object stays until the host closes, a lingering one stays a while, a passing
//! one goes with its last reference and the next reference finds a new object.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:ledger@1.0.0")]
mod ledger {
    /// The balance of a pinned ledger object.
    #[state(service = Pinned)]
    pub struct PinnedBalance {
        pub total: i64,
    }

    /// The balance of a lingering ledger object.
    #[state(service = Lingering)]
    pub struct LingeringBalance {
        pub total: i64,
    }

    /// The balance of a passing ledger object.
    #[state(service = Passing)]
    pub struct PassingBalance {
        pub total: i64,
    }

    /// A ledger whose `main` object the host creates when it installs the Orb and keeps until it
    /// closes; it is acquired only as `main` by the principal `host`.
    #[service(lifetime = host)]
    pub struct Pinned;

    impl Pinned {
        /// Add to the balance and return it.
        pub fn add(amount: i64) -> i64 {
            PinnedBalance::update(|balance| balance.total += amount).total
        }
    }

    /// A ledger whose objects stay two and a half seconds after their last reference is released.
    #[service(linger_ms = 2500)]
    pub struct Lingering;

    impl Lingering {
        /// Add to the balance and return it.
        pub fn add(amount: i64) -> i64 {
            LingeringBalance::update(|balance| balance.total += amount).total
        }
    }

    /// A ledger whose objects live exactly while something references them: the default.
    #[service]
    pub struct Passing;

    impl Passing {
        /// Add to the balance and return it.
        pub fn add(amount: i64) -> i64 {
            PassingBalance::update(|balance| balance.total += amount).total
        }
    }
}
