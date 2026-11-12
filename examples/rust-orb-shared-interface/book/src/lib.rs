//! A compiled Orb implementing an interface it did not write: `example:ledger/book@1.0.0` lives in
//! a WIT package shared with whoever consults a book, and this Orb is one keeper of it. The
//! service names the interface instead of deriving one, and the macro checks the `impl` against
//! the WIT. The entries are kernel-managed state, so a keeper object remembers across reloads.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(no_std, package = "example:keeper@1.0.0", wit = "../wit")]
mod keeper {
    use alloc::vec::Vec;

    /// The entries of one book object, kept by the kernel: the shared package's own records,
    /// which the bindings give `serde`'s derives in an Orb with a state.
    #[state]
    pub struct Entries {
        pub entries: Vec<book::Entry>,
    }

    /// Implements `book` of the shared package: every function of the interface, with the types
    /// of its generated module, `book::Entry`.
    #[service("example:ledger/book@1.0.0")]
    pub struct Keeper;

    impl Keeper {
        /// Add an entry and return how many entries the book holds.
        pub fn add(entry: book::Entry) -> u32 {
            Entries::update(|entries| entries.entries.push(entry))
                .entries
                .len() as u32
        }

        /// The sum of every amount.
        pub fn total() -> i64 {
            Entries::read()
                .entries
                .iter()
                .map(|entry| entry.amount)
                .sum()
        }

        /// The entries, in the order they were added.
        pub fn entries() -> Vec<book::Entry> {
            Entries::read().entries
        }
    }
}
