//! The same WIT implementation with std's runtime or the explicit freestanding runtime. The module
//! `guest` is the default component; a feature selects another module in its place, alone, since
//! a component carries the WIT of one Orb module. With `native`, the selected module's tests
//! install its `orb()` in a native host.
#![cfg_attr(all(target_arch = "wasm32", feature = "no-std"), no_std)]
extern crate alloc;

#[cfg(feature = "configured")]
mod configured;
#[cfg(feature = "contributor")]
mod contributor;
#[cfg(feature = "flagged")]
mod flagged;
#[cfg(feature = "hooks")]
mod hooks;
#[cfg(feature = "io")]
mod io;
#[cfg(feature = "keywords")]
mod keywords;
#[cfg(feature = "lamps")]
mod lamps;
#[cfg(all(test, any(feature = "native", feature = "native-sdk")))]
mod native;
#[cfg(feature = "renamed")]
mod renamed;
#[cfg(feature = "steady")]
mod steady;
#[cfg(feature = "system")]
mod system;
#[cfg(feature = "versioned")]
mod versioned;

#[cfg(feature = "guest")]
#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:runtime@1.0.0",
        wit = "consumed",
        import = "wasi:random/random@0.3.0",
        import = "wasi:clocks/monotonic-clock@0.3.0",
        import("example:counters/counter@1.0.0", keyed),
        import = "example:counters/counter@1.0.0",
        import = "example:ledger/book@1.0.0",
        import = "example:texts/texts@1.0.0",
        import("example:shelves/shelf@1.0.0", keyed),
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
        import("example:counters/counter@1.0.0", keyed),
        import = "example:counters/counter@1.0.0",
        import = "example:ledger/book@1.0.0",
        import = "example:texts/texts@1.0.0",
        import("example:shelves/shelf@1.0.0", keyed)
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

            /// The tally of the object of `key`, the provider's own record, which the view's module
            /// names and this interface `use`s from the provider's.
            pub async fn read(key: String) -> Result<counter_ref::Tally, String> {
                Ok(counter_ref::get(vec![key]).await?.read().await)
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

    /// Reads the `main` object of `example:counters/counter@1.0.0`, which the attribute's plain
    /// `import` reaches, and hands its tallies on as they are: the consumed record crosses this
    /// Orb's derived interface alone, in a list and in a record of the module, and the kernel keeps
    /// the last one in a state.
    pub mod readings {
        use super::counter;
        use alloc::{string::String, vec, vec::Vec};

        /// A tally of the provider, with the note of the reading that took it.
        pub struct Reading {
            pub tally: counter::Tally,
            pub note: String,
        }

        /// The last tally a readings object took, and how many readings it made, kept by the
        /// kernel as the JSON `serde` writes for the provider's record.
        #[state(service = Readings)]
        pub struct Recorded {
            pub last: Option<counter::Tally>,
            pub readings: u32,
            /// How many readings were thrown away: no build holds the field, as `any()` is never
            /// on, and the `Default` the macro writes for the state leaves it out with it.
            #[cfg(any())]
            pub discarded: u32,
        }

        #[service]
        pub struct Readings;

        impl Readings {
            /// Add `amount` to the provider's count, read its tally and keep it.
            pub async fn read(amount: u32, note: String) -> Reading {
                counter::add(amount);
                let tally = counter::read().await;
                Recorded::update(|recorded| {
                    recorded.last = Some(tally.clone());
                    recorded.readings += 1;
                });
                Reading { tally, note }
            }

            /// The counts of the readings and of the tallies, added up.
            pub fn total(readings: Vec<Reading>, tallies: Vec<counter::Tally>) -> u32 {
                readings
                    .iter()
                    .map(|reading| &reading.tally)
                    .chain(&tallies)
                    .map(|tally| tally.count)
                    .sum()
            }

            /// The tally with the parts of its key joined into one.
            pub fn joined(tally: counter::Tally) -> counter::Tally {
                counter::Tally {
                    key: vec![tally.key.join("/")],
                    ..tally
                }
            }
        }
    }

    /// Spells words into the builders of `example:texts/texts@1.0.0`, an interface the attribute
    /// imports plainly, from the package in `consumed/deps/`: a job holds the caller's builder, the
    /// imported interface's handle, in a record of the module, which the runtime moves into the
    /// call as it moves the handle alone. The Orb calls every function of the interface, since the
    /// test reads its contract from the component, which keeps no import function the code never
    /// calls.
    pub mod spelling {
        use super::texts;
        use alloc::string::String;

        /// A word to append to the builder the caller hands over.
        pub struct Job {
            pub builder: texts::Builder,
            pub word: String,
        }

        #[service]
        pub struct Speller;

        impl Speller {
            /// Append the job's word to its builder and return the length of the text; the
            /// builder is consumed.
            pub fn spell_job(job: Job) -> u32 {
                job.builder.append(&job.word)
            }

            /// Spell `word` into a builder of the Orb's own, started through the import and
            /// dropped with the call, and return the length of its text.
            pub fn spell(word: String) -> u32 {
                texts::Builder::new("").append(&word)
            }
        }
    }

    /// Keeps a shelf of `example:shelves/shelf@1.0.0`, an interface of a consumed package this Orb
    /// implements, sorts its books in `library`, a derived interface that takes and returns them
    /// as the shelf's export defines them, and lends them to the shelves of another keeper, which
    /// `lending` reaches by key through the view the macro derives for the attribute's
    /// `import("example:shelves/shelf@1.0.0", keyed)`: the interface this Orb implements.
    pub mod shelves {
        use super::{shelf, shelf_ref};
        use alloc::{string::String, vec, vec::Vec};
        use core::sync::atomic::{AtomicU32, Ordering};

        /// How many books the shelf holds.
        static SHELVED: AtomicU32 = AtomicU32::new(0);

        #[service("example:shelves/shelf@1.0.0")]
        pub struct Shelf;

        impl Shelf {
            pub fn shelve(_book: shelf::Book) -> u32 {
                SHELVED.fetch_add(1, Ordering::Relaxed) + 1
            }
        }

        /// The books of one topic, longest first, and their pages.
        pub struct Topical {
            pub books: Vec<shelf::Book>,
            pub pages: u32,
        }

        #[service]
        pub struct Library;

        impl Library {
            /// The books on `topic`, longest first, with their pages added up.
            pub fn on_topic(books: Vec<shelf::Book>, topic: shelf::Topic) -> Topical {
                let mut books: Vec<shelf::Book> = books
                    .into_iter()
                    .filter(|book| book.topic == topic)
                    .collect();
                books.sort_by(|left, right| right.pages.cmp(&left.pages));
                let pages = books.iter().map(|book| book.pages).sum();
                Topical { books, pages }
            }

            /// The longest book, if any.
            pub fn longest(books: Vec<shelf::Book>) -> Option<shelf::Book> {
                books.into_iter().max_by_key(|book| book.pages)
            }
        }

        /// Lends books to the shelves of another keeper of `example:shelves/shelf@1.0.0`, the
        /// interface this Orb implements, selected by key. The world imports the interface only for
        /// the types the view `use`s, so the installation binds the view alone, and the shelf this
        /// Orb exports stays a service.
        #[service]
        pub struct Lending;

        impl Lending {
            /// Put `books` on the shelf of `key` of the keeper the view reaches, one by one, and
            /// return how many books that shelf holds. A key the keeper refuses is the error.
            pub async fn lend(key: String, books: Vec<shelf::Book>) -> Result<u32, String> {
                let shelf = shelf_ref::get(vec![key]).await?;
                let mut held = 0;
                for book in books {
                    held = shelf.shelve(&lent(book));
                }
                Ok(held)
            }
        }

        /// A book as the view takes it. A derived signature names the shelf's types through the
        /// implemented interface's module, `shelf::Book`, the copy of the export; the view's methods
        /// take the copy of the import, which the view's module names, `shelf_ref::Book`, and whose
        /// `Topic` the bindings' import module of the interface defines.
        fn lent(book: shelf::Book) -> shelf_ref::Book {
            use super::example::shelves::shelf as imported;
            shelf_ref::Book {
                title: book.title,
                pages: book.pages,
                topic: match book.topic {
                    shelf::Topic::Fiction => imported::Topic::Fiction,
                    shelf::Topic::Science => imported::Topic::Science,
                },
            }
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

    /// A tally opened, or why none was: an enum with a case with named fields, which stays this
    /// one, converted at the boundary, the tally it holds included.
    pub enum Opening {
        Opened { tally: Tally, at: u32 },
        Refused { reason: String },
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

        /// Open a tally at `at`, or refuse one above a thousand. The tally crosses in the record
        /// of the case, wrapped into its handle.
        pub fn open(at: u32) -> Opening {
            if at > 1000 {
                Opening::Refused {
                    reason: String::from("above a thousand"),
                }
            } else {
                Opening::Opened {
                    tally: Tally::new(at),
                    at,
                }
            }
        }

        /// Take an opening back: the total of the tally it holds, taken back from its handle, or
        /// none.
        pub fn settle(opening: Opening) -> Option<u32> {
            match opening {
                Opening::Opened { tally, .. } => Some(tally.total.get()),
                Opening::Refused { .. } => None,
            }
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

    /// What a reviewer decided about a text. The case with named fields enters the WIT as a record
    /// of its own, `outcome-refused`; the enum stays this one, converted at the boundary.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Outcome {
        Accepted,
        Refused { reason: String, severity: u32 },
        Deferred(u32),
    }

    /// A decision with its reviewer; it holds the enum, so it stays the author's too.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Decision {
        pub reviewer: String,
        pub outcome: Outcome,
    }

    #[service]
    pub struct Reviews;

    impl Reviews {
        /// Decide on `text` for `reviewer`: an empty text is refused, one under four characters
        /// deferred by the characters it lacks, any other accepted.
        pub fn decide(reviewer: String, text: String) -> Decision {
            let outcome = match text.chars().count() {
                0 => Outcome::Refused {
                    reason: String::from("empty"),
                    severity: 3,
                },
                length if length < 4 => Outcome::Deferred(4 - length as u32),
                _ => Outcome::Accepted,
            };
            Decision { reviewer, outcome }
        }

        /// The refusals among the decisions, each as `reviewer: reason (severity)`.
        pub fn reasons(decisions: Vec<Decision>) -> Vec<String> {
            decisions
                .into_iter()
                .filter_map(|decision| match decision.outcome {
                    Outcome::Refused { reason, severity } => {
                        Some(format!("{}: {reason} ({severity})", decision.reviewer))
                    }
                    _ => None,
                })
                .collect()
        }

        /// The worst of the outcomes, a refusal before a deferral before an acceptance, else
        /// `fallback`; the error when there is neither.
        pub fn worst(outcomes: Vec<Outcome>, fallback: Option<Outcome>) -> Result<Outcome, String> {
            let rank = |outcome: &Outcome| match outcome {
                Outcome::Refused { severity, .. } => 200 + *severity,
                Outcome::Deferred(by) => 100 + *by,
                Outcome::Accepted => 0,
            };
            outcomes
                .into_iter()
                .max_by_key(rank)
                .or(fallback)
                .ok_or_else(|| String::from("no outcome"))
        }
    }

    /// A titled document: free JSON in a record, which the macro keeps the author's, converted to
    /// and from the bindings' record of its text.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Note {
        pub title: String,
        pub body: serde_json::Value,
    }

    /// Free JSON, `serde_json::Value`, crosses as a WIT `string` that holds its text: alone, in a
    /// list, in an option and in a record of the module.
    #[service]
    pub struct Documents;

    impl Documents {
        /// The document, with `note` under the key `note` when it is an object.
        pub fn annotated(document: serde_json::Value, note: String) -> serde_json::Value {
            let mut document = document;
            if let Some(object) = document.as_object_mut() {
                object.insert(String::from("note"), serde_json::Value::String(note));
            }
            document
        }

        /// A note of each document, titled by its position.
        pub fn notes(documents: Vec<serde_json::Value>) -> Vec<Note> {
            documents
                .into_iter()
                .enumerate()
                .map(|(index, body)| Note {
                    title: format!("{index}"),
                    body,
                })
                .collect()
        }

        /// The body of the note when it is an object.
        pub fn object(note: Note) -> Option<serde_json::Value> {
            note.body.is_object().then_some(note.body)
        }
    }

    /// A unit of measure, by its symbol: a newtype, which the WIT spells as a `string`, and which
    /// every symbol makes, through `From`, as a component reads a newtype.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Unit(String);

    impl From<String> for Unit {
        fn from(symbol: String) -> Self {
            Self(symbol)
        }
    }

    /// How many there are: a newtype every `u32` makes, through `From`.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Count(u32);

    impl From<u32> for Count {
        fn from(count: u32) -> Self {
            Self(count)
        }
    }

    /// An amount in a unit: a record holding a newtype, which the macro keeps the author's,
    /// converted to and from the bindings' record of the unit's symbol.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Measure {
        pub amount: f64,
        pub unit: Unit,
    }

    /// A newtype crosses as its field, read back through its `From`: alone, in a list, in a tuple
    /// and in a record of the module.
    #[service]
    pub struct Units;

    impl Units {
        /// The amount in the unit.
        pub fn measured(amount: f64, unit: Unit) -> Measure {
            Measure { amount, unit }
        }

        /// How many of the measures are in the unit, and the units of the others, in order.
        pub fn tally(measures: Vec<Measure>, unit: Unit) -> (Count, Vec<Unit>) {
            let (same, others): (Vec<Measure>, Vec<Measure>) = measures
                .into_iter()
                .partition(|measure| measure.unit == unit);
            let others = others.into_iter().map(|measure| measure.unit).collect();
            (Count::from(same.len() as u32), others)
        }
    }
}

/// A module whose newtype only a refusing `TryFrom` makes, which a component cannot take: it has no
/// error channel for an argument refused before its method runs, only a trap. The
/// `native-only-newtype` feature adds it, and a component build of the crate then fails, naming the
/// newtype, as `executor-wasm/tests/orb_macro_runtime.rs` checks.
#[cfg(feature = "native-only-newtype")]
#[tembra::orb(package = "example:refusing@1.0.0")]
mod refusing {
    use alloc::string::String;

    /// A symbol, which is never empty.
    pub struct Symbol(String);

    impl TryFrom<String> for Symbol {
        type Error = String;

        fn try_from(symbol: String) -> Result<Self, String> {
            if symbol.is_empty() {
                Err(String::from("a symbol is never empty"))
            } else {
                Ok(Self(symbol))
            }
        }
    }

    #[service]
    pub struct Symbols;

    impl Symbols {
        /// The symbol, as given.
        pub fn echo(symbol: Symbol) -> Symbol {
            symbol
        }
    }
}
