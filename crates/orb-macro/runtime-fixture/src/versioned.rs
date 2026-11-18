/// A counter whose state is at version 1: the first revision of the Orb. With the `revised`
/// feature the module is the next revision, below, whose state is at version 2.
#[cfg(not(feature = "revised"))]
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:versioned@1.0.0", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:versioned@1.0.0")
)]
pub mod counting {
    /// The count of one counter object.
    #[state(version = 1)]
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    pub struct Count {
        pub total: u32,
    }

    /// Counts.
    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the total.
        pub fn add(by: u32) -> u32 {
            Count::update(|count| count.total += by).total
        }
    }
}

/// The counter's next revision, whose state is at version 2: it also counts the additions.
#[cfg(feature = "revised")]
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:versioned@1.0.0", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:versioned@1.0.0")
)]
pub mod counting {
    /// The count of one counter object, and how many additions made it.
    #[state(version = 2)]
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    pub struct Count {
        pub total: u32,
        pub additions: u32,
    }

    /// Counts.
    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the total.
        pub fn add(by: u32) -> u32 {
            let count = Count::update(|count| {
                count.total += by;
                count.additions += 1;
            });
            count.total
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::counting;
    use crate::native::{call, hosting, ready};
    use serde_json::{Value, json};

    /// Natively, the revision built keeps its state at the version it declares, 1, or 2 with the
    /// `revised` feature, from the state's `Default`; a reload of the same code keeps the state, at
    /// the same version.
    #[test]
    fn the_state_answers_natively_at_its_version() {
        let revised = cfg!(feature = "revised");
        let (mut pool, host) = hosting(counting::orb().unwrap());
        let counter = ready(&mut pool, &host, "example:versioned/counter@1.0.0");
        let state = counter.state::<Value>().unwrap();
        let add = |pool: &mut _, by: u32| call(pool, &host, &counter, "alice", "add", json!([by]));
        assert_eq!(add(&mut pool, 3), json!(3));
        pool.run_until(host.reload(vec![counting::orb().unwrap()], 5_000))
            .unwrap();
        assert_eq!(add(&mut pool, 1), json!(4));
        let (version, kept) = match revised {
            false => (1, json!({"total": 4})),
            true => (2, json!({"total": 4, "additions": 2})),
        };
        assert_eq!(counter.state_snapshot().unwrap().0.version, version);
        assert_eq!(*state.value().unwrap(), kept);
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
