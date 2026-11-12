//! The same WIT implementation with std's runtime or the explicit freestanding runtime.
#![cfg_attr(all(target_arch = "wasm32", feature = "no-std"), no_std)]
extern crate alloc;

#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:runtime@1.0.0",
        import = "wasi:random/random@0.3.0",
        import = "wasi:clocks/monotonic-clock@0.3.0",
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:runtime@1.0.0",
        import = "wasi:random/random@0.3.0",
        import = "wasi:clocks/monotonic-clock@0.3.0"
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
}
