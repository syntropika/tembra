/// A counter whose crate names the facade `sdk`, which the attribute gives with `crate`, and
/// nothing else: the build of the `renamed` feature has no `tembra`. The `archive` feature adds a
/// method under a `#[cfg]`, so the macro expands the module once per value of the feature, each
/// copy through `::sdk::orb`. The counter keeps a state and reads the call's `context`, whose
/// accessors the component reaches through the facade too.
#[cfg_attr(
    feature = "no-std",
    sdk::orb(package = "example:renamed@1.0.0", crate = "sdk", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    sdk::orb(package = "example:renamed@1.0.0", crate = "sdk", context)
)]
pub mod tally {
    use alloc::{format, string::String};

    /// The count of one counter object, kept by the kernel.
    #[state]
    pub struct Count {
        pub total: u32,
    }

    /// Counts for its callers.
    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and say who added it and what the count is.
        pub fn add(by: u32) -> String {
            let count = Count::update(|count| count.total += by);
            format!("{} counts {}", context::principal(), count.total)
        }

        /// Start the count over and return what it was.
        #[cfg(feature = "archive")]
        pub fn reset() -> u32 {
            let total = Count::read().total;
            Count::update(|count| count.total = 0);
            total
        }
    }
}

#[cfg(all(test, feature = "native-sdk"))]
mod tests {
    use super::tally;
    use crate::native::{call, hosting, ready};
    use serde_json::{Value, json};

    /// Natively too, each copy of the module reaches the facade as `sdk`, the crate's only name for
    /// it: `orb()` serves the counter of the build's features, which keeps its state and reads the
    /// call's principal from the `context`.
    #[test]
    fn a_renamed_facade_serves_the_copies_natively() {
        let (mut pool, host) = hosting(tally::orb().unwrap());
        let counter = ready(&mut pool, &host, "example:renamed/counter@1.0.0");
        let state = counter.state::<Value>().unwrap();
        let mut answer = |method: &'static str, arguments| {
            call(&mut pool, &host, &counter, "alice", method, arguments)
        };
        assert_eq!(answer("add", json!([2])), json!("alice counts 2"));
        assert_eq!(answer("add", json!([3])), json!("alice counts 5"));
        assert_eq!(*state.value().unwrap(), json!({"total": 5}));
        #[cfg(feature = "archive")]
        {
            assert_eq!(answer("reset", json!([])), json!(5));
            assert_eq!(*state.value().unwrap(), json!({"total": 0}));
        }
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
