//! A compiled Orb whose state the kernel keeps. The component holds no count of its own: every
//! `add` reads the state of the object it runs on, changes it, validates it and stores it back,
//! and the kernel retains it across reloads and tells its observers.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:counter@1.0.0")]
mod tally {
    use alloc::{format, string::String};

    /// The state of one counter object: its total and the number of additions made.
    #[state(version = 1)]
    pub struct Count {
        pub total: i64,
        pub steps: u32,
    }

    impl Count {
        /// The rule of this state: a total never goes below zero. Every `update` checks it
        /// before the kernel stores the value, and a reload checks it on the value the new
        /// instance inherits before adopting it.
        pub fn validate(&self) -> Result<(), String> {
            if self.total < 0 {
                return Err(format!("a total of {} is below zero", self.total));
            }
            Ok(())
        }
    }

    /// A counter. The module has one service, so `Count` is the state of its objects.
    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the new count, or the refusal when the total would
        /// go below zero; the kernel then keeps the previous count.
        pub fn add(by: i64) -> Result<Count, String> {
            Count::update(|count| {
                count.total += by;
                count.steps += 1;
            })
        }

        /// The count so far.
        pub fn read() -> Count {
            Count::read()
        }
    }
}
