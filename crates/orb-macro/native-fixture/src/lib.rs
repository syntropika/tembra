//! An Orb crate used natively. The module `calculator` is one `cargo tembra build` would compile
//! into a component; the tests of this crate are a native host instead, which installs
//! `calculator::orb()` and calls its services locally, with Rust values, and portably, with the
//! boundary's JSON. The module `notebook` takes host values, which only a native host can lend.
//! The manifest names every Orb of the crate `calculator` at API version 2, which `orb()` reports.

#[tembra::orb(package = "example:calculator@1.0.0")]
pub mod calculator {
    /// Arithmetic over plain values: numbers, strings, characters, lists, options and tuples.
    #[service]
    pub struct Math;

    impl Math {
        /// The sum of two numbers.
        pub fn add(a: u32, b: u32) -> u32 {
            a + b
        }

        /// The value, negated: 64-bit integers cross the boundary as exact numbers.
        pub fn negate(value: i64) -> i64 {
            -value
        }

        /// The largest of the values, if there is one.
        pub fn largest(values: Vec<u64>) -> Option<u64> {
            values.into_iter().max()
        }

        /// Each value scaled by the factor.
        pub fn scale(values: Vec<f64>, factor: f64) -> Vec<f64> {
            values.into_iter().map(|value| value * factor).collect()
        }

        /// The first character of a text, if any.
        pub fn initial(text: String) -> Option<char> {
            text.chars().next()
        }

        /// A label and its count, the other way round.
        pub fn swap(pair: (String, u32)) -> (u32, String) {
            (pair.1, pair.0)
        }

        /// Whether both hold.
        pub fn both(left: bool, right: bool) -> bool {
            left && right
        }

        /// The sum of the values, awaited.
        pub async fn total(values: Vec<u32>) -> u64 {
            values.into_iter().map(u64::from).sum()
        }

        /// Nothing in, nothing out.
        pub fn ping() {}
    }

    /// A running total for the host's lifetime. The service lives in a sub-module, with its data,
    /// and `orb()` registers it from the Orb module.
    pub mod tally {
        use std::sync::atomic::{AtomicU32, Ordering};

        /// What was recorded so far: a service's data lives in statics, as in the compiled
        /// fixtures.
        static TOTAL: AtomicU32 = AtomicU32::new(0);

        /// A `main` object the host creates when it installs the Orb and keeps until it closes.
        #[service(lifetime = host)]
        pub struct Tally;

        impl Tally {
            /// Add to the total and return it.
            pub fn record(value: u32) -> u32 {
                TOTAL.fetch_add(value, Ordering::SeqCst) + value
            }
        }
    }

    /// Objects that stay a while after their last reference is released, in a private module: the
    /// registration sits next to the service, so the Orb module needs no path to the struct.
    mod memo {
        /// A memo of whatever it is told.
        #[service(linger_ms = 2500)]
        pub struct Memo;

        impl Memo {
            /// The text, as given.
            pub fn echo(text: String) -> String {
                text
            }
        }
    }
}

/// Notes kept for the host, with what the host lends the Orb: the argument of `notebook::orb(..)`.
/// Native only, as host values are.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:notes@1.0.0")]
pub mod notebook {
    use std::{cell::RefCell, rc::Rc};
    use tembra::core::Address;

    /// What the host lends every service of the Orb.
    #[inputs]
    pub struct Inputs {
        /// What happened, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// The title of an object's notes, resolved from its address.
        pub title: Box<dyn Fn(&Address) -> String>,
    }

    /// Notes, as they are given.
    #[service]
    pub struct Notes;

    impl Notes {
        /// The text, as given.
        pub fn echo(text: String) -> String {
            text
        }
    }
}
