//! A crate holding two Orb modules, each with a state, so that each exports the kernel's
//! interfaces of state. Natively it builds, each module an Orb of its own. For wasm32 the second
//! module is refused, since a component holds one Orb.

#[tembra::orb(package = "example:tally@1.0.0")]
pub mod tally {
    /// The count of one counter object.
    #[state]
    pub struct Count {
        pub total: i64,
    }

    /// A counter.
    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the new count.
        pub fn add(by: i64) -> Count {
            Count::update(|count| count.total += by)
        }
    }
}

#[tembra::orb(package = "example:shelf@1.0.0")]
pub mod shelf {
    /// The books of one rack object.
    #[state]
    pub struct Books {
        pub count: u32,
    }

    /// A rack of books.
    #[service]
    pub struct Rack;

    impl Rack {
        /// Put one book on the rack and return its books.
        pub fn put() -> Books {
            Books::update(|books| books.count += 1)
        }
    }
}
