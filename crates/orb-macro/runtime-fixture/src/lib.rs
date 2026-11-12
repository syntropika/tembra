//! The same WIT implementation with std's runtime or the explicit freestanding runtime.
#![cfg_attr(all(target_arch = "wasm32", feature = "no-std"), no_std)]
extern crate alloc;

#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:runtime@1.0.0",
        wit = "consumed",
        import = "wasi:random/random@0.3.0",
        import = "wasi:clocks/monotonic-clock@0.3.0",
        import("example:counters/counter@1.0.0", keyed),
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:runtime@1.0.0",
        wit = "consumed",
        import = "wasi:random/random@0.3.0",
        import = "wasi:clocks/monotonic-clock@0.3.0",
        import("example:counters/counter@1.0.0", keyed)
    )
)]
mod guest {
    use alloc::{format, string::String, vec::Vec};

    #[service]
    pub struct Greeting;

    impl Greeting {
        pub fn hello(name: String) -> String {
            #[cfg(not(feature = "no-std"))]
            {
                let names = std::collections::HashMap::from([("name", name)]);
                format!("Hello, {}!", names["name"])
            }
            #[cfg(feature = "no-std")]
            {
                format!("Hello, {name}!")
            }
        }
    }

    /// A point on a grid.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    /// Where a walker faces; a fresh walker faces north.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub enum Heading {
        #[default]
        North,
        East,
        South,
        West,
    }

    /// One move of a walk.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub enum Move {
        Turn(Heading),
        Step(u32),
        Jump(Point),
        Rest,
    }

    /// Where a walker stands, kept by the kernel. Its derived `Default` is its initial value: at
    /// the origin, facing north, without a move.
    #[state(service = Walker)]
    #[derive(Debug, Default, PartialEq)]
    pub struct Position {
        pub at: Point,
        pub facing: Heading,
        pub moves: u32,
    }

    #[service]
    pub struct Walker;

    impl Walker {
        /// Make the moves from where the walker stands and return where it ends.
        pub fn walk(moves: Vec<Move>) -> Position {
            Position::update(|position| {
                for step in &moves {
                    match step {
                        Move::Turn(heading) => position.facing = *heading,
                        Move::Step(count) => {
                            let count = *count as i32;
                            match position.facing {
                                Heading::North => position.at.y += count,
                                Heading::East => position.at.x += count,
                                Heading::South => position.at.y -= count,
                                Heading::West => position.at.x -= count,
                            }
                        }
                        Move::Jump(to) => position.at = *to,
                        Move::Rest => {}
                    }
                    position.moves += 1;
                }
            })
        }

        /// How many of the moves differ from one another: hashed with std, ordered without it.
        pub fn distinct(moves: Vec<Move>) -> u32 {
            #[cfg(not(feature = "no-std"))]
            let distinct = moves.iter().collect::<std::collections::HashSet<_>>();
            #[cfg(feature = "no-std")]
            let distinct = moves.iter().collect::<alloc::collections::BTreeSet<_>>();
            distinct.len() as u32
        }

        /// Whether the walker stands where a fresh one starts, whatever its moves.
        pub fn at_start() -> bool {
            let position = Position::read();
            let start = Position {
                at: Point::default(),
                facing: Heading::default(),
                moves: position.moves,
            };
            position == start
        }

        /// Where the walker stands, as `Debug` prints it.
        pub fn describe() -> String {
            format!("{:?}", Position::read())
        }
    }

    /// Draws from the host's secure random source and waits on its monotonic clock, through the
    /// WASI interfaces the attribute imports: an installation grants `random` and `clocks`, or the
    /// kernel refuses it.
    #[service]
    pub struct Chance;

    impl Chance {
        /// `length` bytes from the host's secure random source.
        pub fn draw(length: u64) -> Vec<u8> {
            random::get_random_bytes(length)
        }

        /// Wait `nanoseconds` on the host's monotonic clock and return how long the wait took by
        /// that clock.
        pub async fn pause(nanoseconds: u64) -> u64 {
            let start = monotonic_clock::now();
            monotonic_clock::wait_for(nanoseconds).await;
            monotonic_clock::now() - start
        }
    }

    /// Reaches the objects of `example:counters/counter@1.0.0` by key, through the view the macro
    /// derives for the attribute's `import("example:counters/counter@1.0.0", keyed)`.
    pub mod keyed {
        use super::counter_ref;
        use alloc::{format, string::String, vec};

        #[service]
        pub struct Counting;

        impl Counting {
            /// Add `amount` to the object of `key` through two references and to the object of
            /// `other` through one, then read both: the references of one key reach one object,
            /// and another key's object counts on its own. A key the provider refuses is the
            /// error.
            pub async fn count(key: String, other: String, amount: u32) -> Result<String, String> {
                let first = counter_ref::get(vec![key.clone()]).await?;
                let again = counter_ref::get(vec![key]).await?;
                let second = counter_ref::get(vec![other]).await?;
                let added = [first.add(amount), again.add(amount), second.add(amount)];
                let (one, two) = (first.read().await, second.read().await);
                Ok(format!("{added:?} {} {}", tally(&one), tally(&two)))
            }
        }

        /// A tally as `key@principal=count`.
        fn tally(tally: &counter_ref::Tally) -> String {
            format!(
                "{}@{}={}",
                tally.key.join("/"),
                tally.principal,
                tally.count
            )
        }
    }

    /// A running total the caller owns. A function taking it consumes the caller's handle.
    #[wit_resource(service = Tallies)]
    pub struct Tally {
        total: core::cell::Cell<u32>,
    }

    impl Tally {
        #[constructor]
        pub fn new(start: u32) -> Self {
            Self {
                total: core::cell::Cell::new(start),
            }
        }

        /// Add to the total and return it.
        pub fn add(&self, by: u32) -> u32 {
            self.total.set(self.total.get() + by);
            self.total.get()
        }

        /// Add another tally's total to this one and return it; the other tally is consumed.
        pub fn absorb(&self, other: Tally) -> u32 {
            self.add(other.total.get())
        }
    }

    /// How many tallies were destroyed: dropped by their last owner, or consumed by a function
    /// that took them.
    static DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    impl Drop for Tally {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    #[service]
    pub struct Tallies;

    impl Tallies {
        /// Start a tally; the caller owns the returned handle.
        pub fn start(at: u32) -> Tally {
            Tally::new(at)
        }

        /// Take a tally back and return its total.
        pub fn close(tally: Tally) -> u32 {
            tally.total.get()
        }

        /// The totals of the tallies a list and an option hold, in order, each taken back.
        pub fn close_all(tallies: Vec<Tally>, last: Option<Tally>) -> Vec<u32> {
            tallies
                .iter()
                .chain(&last)
                .map(|tally| tally.total.get())
                .collect()
        }

        /// How many tallies were destroyed.
        pub fn dropped() -> u32 {
            DROPPED.load(core::sync::atomic::Ordering::Relaxed)
        }
    }

    /// What a holder may do with a document: each field is a flag of the WIT `flags`. The struct
    /// stays this one; the bindings take it as it is.
    #[flags]
    #[derive(Default, PartialEq)]
    pub struct Access {
        pub read: bool,
        pub write: bool,
        pub share: bool,
    }

    /// The access granted to someone.
    pub struct Grant {
        pub who: String,
        pub access: Access,
    }

    /// The last access a permits object granted and how many grants it made, kept by the kernel.
    #[state(service = Permits)]
    pub struct Granted {
        pub last: Access,
        pub grants: u32,
    }

    #[service]
    pub struct Permits;

    impl Permits {
        /// The access with exactly the flags `access` lacks.
        pub fn invert(access: Access) -> Access {
            Access {
                read: !access.read,
                write: !access.write,
                share: !access.share,
            }
        }

        /// Grant `access` to `who`; the kernel keeps it as the last access granted.
        pub fn give(who: String, access: Access) -> Grant {
            Granted::update(|granted| {
                granted.last = access;
                granted.grants += 1;
            });
            Grant { who, access }
        }

        /// The flags any of the grants or `extra` sets.
        pub fn merged(grants: Vec<Grant>, extra: Option<Access>) -> Access {
            grants
                .iter()
                .map(|grant| grant.access)
                .chain(extra)
                .fold(Access::default(), |all, access| all | access)
        }

        /// What the kernel keeps for this object.
        pub fn kept() -> Granted {
            Granted::read()
        }
    }
}
