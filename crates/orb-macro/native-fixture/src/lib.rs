//! An Orb crate used natively. The module `calculator` is one `cargo tembra build` would compile
//! into a component; the tests of this crate are a native host instead, which installs
//! `calculator::orb()` and calls its services locally, with Rust values, and portably, with the
//! boundary's JSON, and whose methods read their call through the kernel's `context`, their
//! object's state and the resources of the Orb's root, as a component's do. The module `keeper`
//! implements an interface of the package in `wit/`, whose types are the bindings'. The module
//! `notebook` takes host values, which only a native host can lend, `library` shares its root's
//! value among its objects, `worker` only runs its root and a resource of it, and `pier` gives the
//! objects a create builds a resource each. The module
//! `metering` imports the meter interface of the package in `wit/deps/`, which `meters` implements,
//! and its `orb(..)` takes the provider of each import; `exchange` imports an interface of types
//! only, which needs none; `filter` contributes to the hook of a gate in `wit/deps/`, and its
//! `orb(..)` takes the gate. The module `counters` rewrites with the macro a native Orb its test
//! also writes by hand against the runtime's API. `abacus` and `restrung` are one Orb before and
//! after a reload that gives its root's resource a new version, and `reframed` that Orb after a
//! reload that gives only its package a new version. `cistern` and `refilled` are one Orb before
//! and after a reload that gives a resource's dependency a new version. `dice`, `checkpoint` and `desk`
//! hold what the native side does not cover yet, a WASI import, a hook
//! declaration and a record holding a resource: they compile here all the same, and their `orb()`
//! answers an error naming it. `trail` holds its root two private modules down, with a service in a
//! module below it, and a service whose method is named `clone`, as a method of `Rc`. `hollow`
//! holds a service in a private module, its state in a module below it and its WIT resource, with a
//! constructor and a static function, in another, and re-exports the client and the handle where
//! the runtime is linked. The manifest names every Orb of the crate `calculator` at API version 2,
//! which `orb()` reports.
//!
//! A host that leaves a provider out of `metering::Providers` does not compile:
//!
//! ```compile_fail,E0063
//! use tembra::Provider;
//! use tembra_orb_native_fixture::metering;
//!
//! let orb = metering::orb(metering::Providers {
//!     gauge: Provider::new("meters", 1),
//! });
//! ```

use std::cell::{Cell, RefCell};

std::thread_local! {
    /// How far from the origin, along each axis, the walkers of the Orbs this thread hosts may
    /// stand: the rule of the walker's state reads it, so a test narrows it before a reload.
    static GRID: Cell<i64> = const { Cell::new(1000) };
}

/// Set how far from the origin, along each axis, the walkers of the Orbs this thread hosts may
/// stand: 1000 steps, unless a test narrows it.
pub fn set_grid(steps: i64) {
    GRID.with(|grid| grid.set(steps));
}

std::thread_local! {
    /// What the resources of the Orbs' roots went through on this thread, in order: the tests'
    /// host opens and closes them on the thread that installs the Orbs.
    static JOURNAL: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Note what a resource went through.
fn note(event: String) {
    JOURNAL.with(|journal| journal.borrow_mut().push(event));
}

/// What the resources of the Orbs' roots went through on this thread since the last call, in
/// order: each opened and closed, with what it held.
pub fn journal() -> Vec<String> {
    JOURNAL.with(RefCell::take)
}

std::thread_local! {
    /// While a test holds them, the accumulators' `add_later` calls on this thread wait at their
    /// await, with the wakers of those waiting there; `None` lets them pass.
    static HELD: RefCell<Option<Vec<std::task::Waker>>> = const { RefCell::new(None) };
}

/// Hold each accumulator's `add_later` on this thread at its await, until `release_additions`.
pub fn hold_additions() {
    HELD.with(|held| {
        held.borrow_mut().get_or_insert_with(Vec::new);
    });
}

/// How many `add_later` calls wait at their await on this thread.
pub fn held_additions() -> usize {
    HELD.with(|held| held.borrow().as_ref().map_or(0, Vec::len))
}

/// Let the `add_later` calls waiting at their await on this thread go on, and the next ones pass.
pub fn release_additions() {
    let waiting = HELD.with(|held| held.borrow_mut().take());
    waiting
        .into_iter()
        .flatten()
        .for_each(std::task::Waker::wake);
}

/// Wait while `hold_additions` holds the calls on this thread.
async fn additions_released() {
    std::future::poll_fn(|context| {
        HELD.with(|held| match &mut *held.borrow_mut() {
            Some(waiting) => {
                if !waiting.iter().any(|waker| waker.will_wake(context.waker())) {
                    waiting.push(context.waker().clone());
                }
                std::task::Poll::Pending
            }
            None => std::task::Poll::Ready(()),
        })
    })
    .await
}

#[tembra::orb(package = "example:calculator@1.0.0", context)]
pub mod calculator {
    pub mod math {
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

            /// Zero, the value a calculation starts from. Its client calls it `new_`, since `new`
            /// builds the client.
            #[allow(clippy::new_ret_no_self)]
            pub fn new() -> u32 {
                0
            }

            /// A value labelled with the context it was computed in. The client names the call's
            /// context `context` too, and keeps the two apart.
            pub fn label(value: u32, context: String) -> String {
                format!("{context}: {value}")
            }

            /// The ratio of two numbers, which the boundary carries only when finite and not `-0`.
            pub fn ratio(dividend: f32, divisor: f32) -> f32 {
                dividend / divisor
            }
        }
    }

    /// A point of the plotter's grid: 64-bit coordinates cross the boundary as exact numbers.
    #[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct Point {
        pub x: i64,
        pub y: i64,
    }

    /// Where the pen moves.
    #[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
    pub enum Heading {
        North,
        East,
        South,
        West,
    }

    /// What the plotter draws: a case without a value, one with a value, and one with named fields,
    /// which carries a record of its own.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Shape {
        Dot,
        Square(u32),
        Rect { width: u32, height: u32 },
    }

    /// How a shape is drawn: a record of `bool` fields.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Pen {
        pub filled: bool,
        pub dashed_line: bool,
    }

    /// The moves of a walk, in a private module: their JSON is written there, and the Orb module's
    /// services name them by path.
    mod moves {
        /// One move of a walk: a case with a value, one with another module type, one without.
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub enum Move {
            Turn(super::Heading),
            Step(u32),
            Stay,
        }
    }

    pub use moves::Move;

    /// A shape drawn with a pen: it holds an enum the macro keeps the author's, so it stays the
    /// author's too.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Stroke {
        pub shape: Shape,
        pub pen: Pen,
    }

    pub mod plotter {
        use super::{Heading, Pen, Point, Shape, Stroke, moves};

        /// Plots on a grid, with records, enums, variants, options, results and 64-bit integers.
        #[service]
        pub struct Plotter;

        impl Plotter {
            /// The point `steps` away towards the heading, or the point itself without one.
            pub fn step(from: Point, heading: Option<Heading>, steps: u64) -> Point {
                let steps = steps as i64;
                match heading {
                    None => from,
                    Some(Heading::North) => Point {
                        y: from.y + steps,
                        ..from
                    },
                    Some(Heading::East) => Point {
                        x: from.x + steps,
                        ..from
                    },
                    Some(Heading::South) => Point {
                        y: from.y - steps,
                        ..from
                    },
                    Some(Heading::West) => Point {
                        x: from.x - steps,
                        ..from
                    },
                }
            }

            /// Where a walk from the point ends, heading north until it turns.
            pub fn walk(from: Point, path: Vec<moves::Move>) -> Point {
                let mut heading = Heading::North;
                let mut at = from;
                for step in path {
                    match step {
                        moves::Move::Turn(to) => heading = to,
                        moves::Move::Step(count) => {
                            at = Self::step(at, Some(heading), u64::from(count));
                        }
                        moves::Move::Stay => {}
                    }
                }
                at
            }

            /// The area of a shape, or why it has none.
            pub fn area(shape: Shape) -> Result<u64, String> {
                match shape {
                    Shape::Dot => Err("a dot has no area".to_string()),
                    Shape::Square(side) => Ok(u64::from(side) * u64::from(side)),
                    Shape::Rect { width, height } => Ok(u64::from(width) * u64::from(height)),
                }
            }

            /// The area, or the fallback when there is none.
            pub fn settled(area: Result<u64, String>, fallback: u64) -> u64 {
                area.unwrap_or(fallback)
            }

            /// The stroke with its shape scaled by the factor and each field of its pen flipped.
            pub fn redrawn(stroke: Stroke, factor: u32) -> Stroke {
                let shape = match stroke.shape {
                    Shape::Dot => Shape::Dot,
                    Shape::Square(side) => Shape::Square(side * factor),
                    Shape::Rect { width, height } => Shape::Rect {
                        width: width * factor,
                        height: height * factor,
                    },
                };
                let pen = Pen {
                    filled: !stroke.pen.filled,
                    dashed_line: !stroke.pen.dashed_line,
                };
                Stroke { shape, pen }
            }

            /// Which of the points lie on a heading's axis from the origin, in order, and how many
            /// do not: a list of options and a result without values.
            pub fn sorted(points: Vec<Point>) -> (Vec<Option<Heading>>, Result<(), u32>) {
                let headings: Vec<Option<Heading>> = points
                    .iter()
                    .map(|point| match (point.x.signum(), point.y.signum()) {
                        (0, 1) => Some(Heading::North),
                        (1, 0) => Some(Heading::East),
                        (0, -1) => Some(Heading::South),
                        (-1, 0) => Some(Heading::West),
                        _ => None,
                    })
                    .collect();
                let off = headings.iter().filter(|heading| heading.is_none()).count() as u32;
                let result = if off == 0 { Ok(()) } else { Err(off) };
                (headings, result)
            }
        }
    }

    pub mod walker {
        use super::plotter::Plotter;
        use super::{Heading, Point, context, moves};

        /// Where a walker stands, which the kernel keeps for each walker object: a method reads and
        /// updates it through the kernel, natively as in a component. The kernel keeps it as the
        /// JSON serde writes, the same in both, which is that of the runtime fixture's walker:
        /// `{"at": {"x": 0, "y": 0}, "facing": "North", "moves": 0}` to start with.
        #[state]
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct Position {
            pub at: Point,
            pub facing: Heading,
            pub moves: u32,
        }

        /// A walker starts at the origin, facing north, without a move.
        impl Default for Position {
            fn default() -> Self {
                Position {
                    at: Point { x: 0, y: 0 },
                    facing: Heading::North,
                    moves: 0,
                }
            }
        }

        impl Position {
            /// The rule of the state: a walker stays on the grid, at most 1000 steps from the
            /// origin along each axis, or as many as `set_grid` says. The kernel keeps where it
            /// stood when an update breaks it, and the running code when a reload inherits a walker
            /// it breaks.
            #[validate]
            pub fn on_grid(&self) -> Result<(), String> {
                let steps = super::super::GRID.with(std::cell::Cell::get);
                if self.at.x.abs() > steps || self.at.y.abs() > steps {
                    return Err(format!("({}, {}) is off the grid", self.at.x, self.at.y));
                }
                Ok(())
            }
        }

        /// Walks the plotter's grid: the kernel keeps where each walker object stands.
        #[service]
        pub struct Walker;

        impl Walker {
            /// Make the moves from where the walker stands, heading as it faces, and return where
            /// it ends, or why the kernel keeps it where it stood.
            pub fn walk(path: Vec<moves::Move>) -> Result<Position, String> {
                Position::update(|position| {
                    for step in &path {
                        match *step {
                            moves::Move::Turn(heading) => position.facing = heading,
                            moves::Move::Step(count) => {
                                position.at = Plotter::step(
                                    position.at,
                                    Some(position.facing),
                                    u64::from(count),
                                );
                            }
                            moves::Move::Stay => {}
                        }
                        position.moves += 1;
                    }
                })
            }

            /// Where the walker stands.
            pub fn here() -> Position {
                Position::read()
            }

            /// Cancel the call, then make the moves: the kernel refuses the value of a cancelled
            /// call and keeps where the walker stood, which `update` answers with, as in a
            /// component.
            pub fn stumble(path: Vec<moves::Move>) -> Result<Position, String> {
                context::cancel();
                Self::walk(path)
            }
        }
    }

    /// The calculator's memory, a resource of the Orb's root: the host opens it when it installs
    /// the Orb and closes it when it removes the Orb, and every object of the Orb uses the one
    /// value, natively as in a component.
    #[resource]
    pub struct Memory {
        value: i64,
    }

    impl Memory {
        /// Clear the memory, as the host opens it.
        #[open]
        fn open() -> Self {
            super::note("memory open".to_owned());
            Memory { value: 0 }
        }

        /// Note what the memory held, as the host closes it.
        #[close]
        fn close(self) {
            super::note(format!("memory closed at {}", self.value));
        }
    }

    pub mod register {
        use super::Memory;

        /// The memory keys of the calculator, on the memory of the Orb's root.
        #[service]
        pub struct Register;

        impl Register {
            /// Add the value to the memory and return what it holds.
            pub fn add(value: i64) -> i64 {
                Memory::with(|memory| {
                    memory.value += value;
                    memory.value
                })
            }

            /// What the memory holds.
            pub fn recall() -> i64 {
                Memory::with(|memory| memory.value)
            }
        }
    }

    pub mod caller {
        use super::context;

        /// What the kernel tells a method of its call, through the functions of `context`: in a
        /// component as in a native host.
        #[service]
        pub struct Caller;

        impl Caller {
            /// Who calls and which object answers: the principal before and after the call waits,
            /// then the key of the object and the principal that acquired it.
            pub async fn whoami() -> (String, String, Vec<String>, String) {
                let before = context::principal();
                waited().await;
                let object = context::object();
                (before, context::principal(), object.key, object.principal)
            }

            /// Write the message to the host's log at level warn, once the call has waited.
            pub async fn warn(message: String) {
                waited().await;
                context::log(context::Level::Warn, &message);
            }

            /// Once the call has waited, whether it reads as cancelled before and after it cancels
            /// itself, and its principal, which it keeps once cancelled.
            pub async fn give_up() -> (bool, bool, String) {
                waited().await;
                let before = context::cancelled();
                context::cancel();
                (before, context::cancelled(), context::principal())
            }

            /// Wait until the caller cancels the call, then cancel it from inside: a cancelled
            /// call's cancel changes nothing, so its root and the root's other calls go on.
            pub async fn abandon() {
                while !context::cancelled() {
                    waited().await;
                }
                context::cancel();
            }
        }

        /// Wait once: pending on the first poll, so the method resumes in a later one, after the
        /// executor has polled whatever else is ready.
        async fn waited() {
            let mut first = true;
            std::future::poll_fn(|cx| {
                if std::mem::take(&mut first) {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .await;
        }
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

        /// A titled document: free JSON in a record, which the macro keeps the author's.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Note {
            pub title: String,
            pub body: serde_json::Value,
        }

        impl Memo {
            /// The text, as given.
            pub fn echo(text: String) -> String {
                text
            }

            /// The document, with `note` under the key `note` when it is an object: free JSON
            /// crosses as a WIT `string` that holds its text.
            pub fn annotated(document: serde_json::Value, note: String) -> serde_json::Value {
                let mut document = document;
                if let Some(object) = document.as_object_mut() {
                    object.insert("note".to_string(), serde_json::Value::String(note));
                }
                document
            }

            /// A note of each document, titled by its position.
            pub fn notes(documents: Vec<serde_json::Value>) -> Vec<Note> {
                documents
                    .into_iter()
                    .enumerate()
                    .map(|(index, body)| Note {
                        title: index.to_string(),
                        body,
                    })
                    .collect()
            }
        }
    }

    pub use memo::Note;

    /// Running totals their callers own, in a module of their own: `Accumulator` is a WIT resource
    /// of the module's only service, which a caller constructs or opens, adds to through its
    /// handle, and drops.
    pub mod accumulators {
        use std::cell::Cell;

        thread_local! {
            /// How many accumulators were dropped on this thread: a test's host runs on its own.
            static DROPPED: Cell<u32> = const { Cell::new(0) };
        }

        /// A running total: its caller holds a handle, and the value stays with the Orb's object
        /// that issued it.
        #[wit_resource]
        pub struct Accumulator {
            total: Cell<i64>,
        }

        impl Accumulator {
            /// An accumulator starting at `start`: the resource's constructor.
            #[constructor]
            pub fn new(start: i64) -> Accumulator {
                Accumulator {
                    total: Cell::new(start),
                }
            }

            /// Add to the total and return it.
            pub fn add(&self, value: i64) -> i64 {
                let total = self.total.get() + value;
                self.total.set(total);
                total
            }

            /// Add to the total once the call goes on, and return the total before the wait and
            /// after the addition: an async method, which reads `self` on both sides of its
            /// await, where a host may hold it with `hold_additions`.
            pub async fn add_later(&self, value: i64) -> (i64, i64) {
                let before = self.total.get();
                crate::additions_released().await;
                let total = self.total.get() + value;
                self.total.set(total);
                (before, total)
            }

            /// The total so far.
            pub fn total(&self) -> i64 {
                self.total.get()
            }

            /// The principal of the call reading it: a method of the resource reads its call
            /// through the kernel's `context`, as a service's methods do.
            pub fn reader(&self) -> String {
                super::context::principal()
            }

            /// Add the total to the calculator's memory, a resource of the Orb's root, and count
            /// it in the state of the accumulators object that issued it: what the memory holds,
            /// and how many totals that object stored. A method of the resource reaches both, as
            /// a method of the service does.
            pub fn store(&self) -> (i64, u32) {
                let memory = super::Memory::with(|memory| {
                    memory.value += self.total.get();
                    memory.value
                });
                let stored = Stored::update(|stored| stored.count += 1);
                (memory, stored.count)
            }

            /// An accumulator starting at zero: a static function of the resource.
            pub fn zero() -> Accumulator {
                Accumulator::new(0)
            }
        }

        impl Drop for Accumulator {
            fn drop(&mut self) {
                DROPPED.with(|dropped| dropped.set(dropped.get() + 1));
            }
        }

        /// How many totals the accumulators an object of the service issued stored in the
        /// calculator's memory, which the kernel keeps for the object.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Stored {
            pub count: u32,
        }

        /// Opens accumulators and counts the ones dropped.
        #[service]
        pub struct Accumulators;

        impl Accumulators {
            /// An accumulator for each start, and how many there are: owned results in a list and
            /// a tuple.
            pub fn open(starts: Vec<i64>) -> (Vec<Accumulator>, u32) {
                let count = starts.len() as u32;
                (starts.into_iter().map(Accumulator::new).collect(), count)
            }

            /// An accumulator when the start is not negative, or why not: an owned result in a
            /// `Result`.
            pub fn checked(start: i64) -> Result<Accumulator, String> {
                if start < 0 {
                    Err(format!("{start} is negative"))
                } else {
                    Ok(Accumulator::new(start))
                }
            }

            /// How many accumulators were dropped.
            pub fn dropped() -> u32 {
                DROPPED.with(Cell::get)
            }
        }
    }

    /// A unit of measure, by its symbol: a newtype, which the WIT spells as a `string`, and which
    /// every symbol makes, through `From`, as a module a component build takes needs.
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

    /// An amount in a unit: a record holding a newtype, which the macro keeps the author's.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Measure {
        pub amount: f64,
        pub unit: Unit,
    }

    pub mod units {
        use super::{Count, Measure, Unit};

        /// Measures in units: each newtype crosses as its field, read back through its `From`.
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
}

/// Keeps a book of `example:ledger/book@1.0.0`, an interface of the package in `wit/` this Orb
/// implements: its record, variant, enums and flags are the types the bindings generate from that
/// WIT, which a native caller passes as they are and a portable one as the boundary's JSON.
#[tembra::orb(package = "example:keeper@1.0.0", wit = "wit")]
pub mod keeper {
    pub mod bookkeeping {
        use super::book;
        use std::cell::RefCell;

        std::thread_local! {
            /// The entries recorded so far, in order: a service's data lives outside its objects,
            /// as in the compiled fixtures.
            static ENTRIES: RefCell<Vec<book::Entry>> = const { RefCell::new(Vec::new()) };
        }

        #[service("example:ledger/book@1.0.0")]
        pub struct Keeper;

        impl Keeper {
            /// Record the entry, unless it moves no amount, and answer the balance after it.
            pub fn add(entry: book::Entry) -> book::Outcome {
                if entry.amount == 0 {
                    return book::Outcome::Refused(format!("`{}` moves no amount", entry.label));
                }
                ENTRIES.with(|entries| {
                    let mut entries = entries.borrow_mut();
                    entries.push(entry);
                    book::Outcome::Recorded(entries.iter().map(signed).sum())
                })
            }

            /// The entries recorded in the direction, or all of them, in order.
            pub fn entries(direction: Option<book::Direction>) -> Vec<book::Entry> {
                ENTRIES.with(|entries| {
                    entries
                        .borrow()
                        .iter()
                        .filter(|entry| {
                            direction.is_none_or(|direction| entry.direction == direction)
                        })
                        .cloned()
                        .collect()
                })
            }

            /// The last entries recorded, at most `count` of them, in order: the interface's alias
            /// of a list of entries, which the bindings name `book::Journal`.
            pub fn recent(count: u32) -> book::Journal {
                ENTRIES.with(|entries| {
                    let entries = entries.borrow();
                    let from = entries.len().saturating_sub(count as usize);
                    entries[from..].to_vec()
                })
            }
        }

        /// The amount an entry adds to the balance: its amount for a credit, the opposite for a
        /// debit.
        fn signed(entry: &book::Entry) -> i64 {
            let amount = entry.amount as i64;
            match entry.direction {
                book::Direction::Credit => amount,
                book::Direction::Debit => -amount,
            }
        }
    }
}

/// Notes kept for the host, with what the host lends the Orb: the argument of `notebook::orb(..)`.
/// Native only, as host values, the objects a create builds, a method's `&Context` and a `#[local]`
/// method's closure are. The crate's `archive` feature adds an input, the Orb's root, a method of
/// `Notes` and the sub-module `shelf` with its service, under `#[cfg]`s the macro evaluates.
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
        /// Where archived notes go, with the `archive` feature: the compiler evaluates this
        /// `#[cfg]`, as on any struct, since `orb(..)` takes the inputs whole.
        #[cfg(feature = "archive")]
        pub archive: Rc<RefCell<Vec<String>>>,
    }

    /// The host's archive, as the Orb's root shares it with every object.
    #[cfg(feature = "archive")]
    pub struct Archive(Rc<RefCell<Vec<String>>>);

    /// Share the host's archive with the Orb's objects, once: the Orb's root, which the `archive`
    /// feature adds.
    #[cfg(feature = "archive")]
    #[root]
    fn share(inputs: &Inputs, _: &tembra::Declaration) -> tembra::Result<Archive> {
        Ok(Archive(inputs.archive.clone()))
    }

    pub mod notes {
        use super::Inputs;
        use std::cell::RefCell;
        use tembra::{Declaration, Resource};

        /// The notes of one object, which its create builds once per key and principal.
        #[service]
        pub struct Notes {
            title: String,
            notes: RefCell<Vec<String>>,
            journal: Resource<String>,
            #[cfg(feature = "archive")]
            archive: std::rc::Rc<RefCell<Vec<String>>>,
        }

        impl Notes {
            /// Build an object: its title, which the host resolves from its address, and what it
            /// declares, each reported to the host: its state, the object's own label; its journal,
            /// a resource opened once this returns and closed with the object; and a task, run once
            /// the object is active.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                let address = declaration.address()?;
                let object = format!("{}/{}", address.key.join("/"), address.principal);
                inputs.events.borrow_mut().push(format!("create {object}"));
                declaration.state(object.clone(), 1, |_| Ok(()))?;
                let (opened, closed) = (inputs.events.clone(), inputs.events.clone());
                let label = object.clone();
                let journal = declaration.resource(
                    "journal",
                    1,
                    move || {
                        opened.borrow_mut().push(format!("open {label}"));
                        Ok(format!("journal of {label}"))
                    },
                    move |journal, _| {
                        Box::pin(async move {
                            closed.borrow_mut().push(format!("close {journal}"));
                            Ok(())
                        })
                    },
                )?;
                let ran = inputs.events.clone();
                declaration.task(move |_| {
                    Box::pin(async move {
                        ran.borrow_mut().push(format!("task {object}"));
                        Ok(())
                    })
                })?;
                Ok(Self {
                    title: (inputs.title)(&address),
                    notes: RefCell::default(),
                    journal,
                    #[cfg(feature = "archive")]
                    archive: declaration.orb::<super::Archive>()?.0.clone(),
                })
            }

            /// Add a note to the object's notes and return how many it holds.
            pub fn add(&self, text: String) -> u32 {
                let mut notes = self.notes.borrow_mut();
                notes.push(text);
                notes.len() as u32
            }

            /// The title of the object's notes.
            pub fn title(&self) -> String {
                self.title.clone()
            }

            /// The object's journal: the value its resource opened with.
            pub fn journal(&self) -> Option<String> {
                self.journal.value().ok().map(|journal| journal.to_string())
            }

            /// The note at the position, or the call's error when there is none: a `tembra::Result`
            /// is the method's error channel, and its WIT result is the note alone.
            pub fn note(&self, position: u32) -> tembra::Result<String> {
                self.notes
                    .borrow()
                    .get(position as usize)
                    .cloned()
                    .ok_or_else(|| {
                        tembra::Error::invalid("no-note", format!("there is no note {position}"))
                    })
            }

            /// Where the note is, its absence a value: any other `Result` is a WIT `result`.
            pub fn position(&self, text: String) -> Result<u32, String> {
                self.notes
                    .borrow()
                    .iter()
                    .position(|note| *note == text)
                    .map(|position| position as u32)
                    .ok_or_else(|| format!("no note {text}"))
            }

            /// The title with the journal, awaited, through `?` on the object's environment, which
            /// needs the error channel.
            pub async fn heading(&self) -> tembra::Result<String> {
                let journal = self.journal.value()?;
                Ok(format!("{}: {journal}", self.title))
            }

            /// Nothing, when the object holds at least `count` notes; the call fails otherwise.
            pub fn holds(&self, count: u32) -> tembra::Result<()> {
                let held = self.notes.borrow().len() as u32;
                if held >= count {
                    Ok(())
                } else {
                    Err(tembra::Error::invalid(
                        "too-few",
                        format!("{held} notes, not {count}"),
                    ))
                }
            }

            /// Move the object's notes to the host's archive and return how many moved: a method
            /// the `archive` feature adds to the interface.
            #[cfg(feature = "archive")]
            pub fn archive(&self) -> u32 {
                let notes = self.notes.take();
                let moved = notes.len() as u32;
                self.archive.borrow_mut().extend(notes);
                moved
            }
        }
    }

    pub mod board {
        use super::Inputs;
        use std::{cell::RefCell, rc::Rc};
        use tembra::Declaration;

        /// The board the host pins notes on: one object, of key `main` and principal `host`, which
        /// its factory guards.
        #[service(key = main, principal = "host")]
        pub struct Board {
            events: Rc<RefCell<Vec<String>>>,
        }

        impl Board {
            /// Report the board built: an address the guard refuses never gets here.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                let address = declaration.address()?;
                inputs.events.borrow_mut().push(format!(
                    "board {}/{}",
                    address.key.join("/"),
                    address.principal
                ));
                Ok(Self {
                    events: inputs.events.clone(),
                })
            }

            /// Pin a note on the board.
            pub fn pin(&self, text: String) {
                self.events.borrow_mut().push(format!("pin {text}"));
            }
        }
    }

    pub mod session {
        use super::Tag;
        use std::rc::Rc;
        use tembra::Context;

        /// What a call carries besides its arguments: a method that takes `&Context` reads the
        /// call's context, which a native host passes and the WIT leaves out.
        #[service]
        pub struct Session;

        impl Session {
            /// The principal of the caller's root.
            pub fn caller(context: &Context) -> String {
                context.principal().unwrap_or_default()
            }

            /// The value through an operation the caller hands over: a closure, which only exists
            /// natively and has no serde, so the method is `#[local]` and the WIT leaves it out.
            #[local]
            pub fn apply(value: u32, operation: Rc<dyn Fn(u32) -> u32>) -> u32 {
                operation(value)
            }

            /// The tag, marked as one.
            pub fn tagged(tag: Tag) -> String {
                format!("#{}", tag.0)
            }
        }
    }

    /// A tag of notes: a newtype, which the WIT spells as a `string`, whose `TryFrom` refuses a tag
    /// that is empty or holds whitespace. Only a native host takes such a newtype, where a value it
    /// refuses fails that call alone: a component would have to trap.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Tag(String);

    impl TryFrom<String> for Tag {
        type Error = String;

        fn try_from(tag: String) -> Result<Self, String> {
            if tag.is_empty() || tag.contains(char::is_whitespace) {
                Err(format!("`{tag}` is no tag"))
            } else {
                Ok(Self(tag))
            }
        }
    }

    /// The service of the host's archive, in a sub-module the `archive` feature adds, whose
    /// `#[cfg]` takes the service with it.
    #[cfg(feature = "archive")]
    pub mod shelf {
        use super::{Archive, Inputs};
        use std::{cell::RefCell, rc::Rc};
        use tembra::Declaration;

        /// The host's archive of every object's notes.
        #[service]
        pub struct Shelf {
            archive: Rc<RefCell<Vec<String>>>,
        }

        impl Shelf {
            /// Read the archive the Orb's root shares.
            #[create]
            fn create(_: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                Ok(Self {
                    archive: declaration.orb::<Archive>()?.0.clone(),
                })
            }

            /// How many notes the archive holds.
            pub fn count(&self) -> u32 {
                self.archive.borrow().len() as u32
            }
        }
    }
}

/// Shelves of books, with what every shelf shares: the catalogue the Orb's root creates once, which
/// each shelf's create and methods read; what each shelf holds, which the kernel keeps for it; and
/// porters, objects without methods that run a task. Native only, as `notebook`. The module holds
/// the bindings of the kernel's `tembra:guest` package, for the shelves' state, under the name
/// `tembra`, so it names the facade `::tembra`.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:library@1.0.0")]
pub mod library {
    use ::tembra::Declaration;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    /// What the host lends the Orb.
    #[inputs]
    pub struct Inputs {
        /// What happened, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
    }

    /// What every shelf shares: how many books the library holds.
    pub struct Catalogue {
        books: Cell<u32>,
    }

    /// Create the catalogue, once for the Orb, and report it to the host.
    #[root]
    fn catalogue(inputs: &Inputs, _declaration: &Declaration) -> ::tembra::Result<Catalogue> {
        inputs.events.borrow_mut().push("root".to_string());
        Ok(Catalogue {
            books: Cell::new(0),
        })
    }

    pub mod shelf {
        use super::{Catalogue, Inputs};
        use ::tembra::{Context, Declaration};
        use std::rc::Rc;

        /// One shelf of the library, an object per key and principal.
        #[service]
        pub struct Shelf {
            catalogue: Rc<Catalogue>,
        }

        /// How many books one shelf holds, which the kernel keeps for each shelf.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Shelved {
            pub books: u32,
        }

        impl Shelf {
            /// Build a shelf on the catalogue the root created, and report how many books the
            /// library held then.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> ::tembra::Result<Self> {
                let catalogue = declaration.orb::<Catalogue>()?;
                let address = declaration.address()?;
                inputs.events.borrow_mut().push(format!(
                    "shelf {}/{} with {}",
                    address.key.join("/"),
                    address.principal,
                    catalogue.books.get()
                ));
                Ok(Self { catalogue })
            }

            /// Shelve a book, count it on this shelf, and return how many books the library holds,
            /// across its shelves.
            pub fn shelve(&self) -> u32 {
                Shelved::update(|shelved| shelved.books += 1);
                let books = self.catalogue.books.get() + 1;
                self.catalogue.books.set(books);
                books
            }

            /// How many books the library holds, read from the catalogue through the call's
            /// context, as a method that keeps nothing of the root reads it.
            pub fn books(context: &Context) -> u32 {
                context
                    .orb::<Catalogue>()
                    .map_or(0, |catalogue| catalogue.books.get())
            }
        }
    }

    pub mod porter {
        use super::Inputs;
        use ::tembra::Declaration;

        /// A porter who tidies the library while someone keeps one at hand: its objects have no
        /// method, only the task their create declares.
        #[service]
        pub struct Porter;

        impl Porter {
            /// Declare the porter's round: a task reported when it starts, once the object is
            /// active, and when it is cancelled, as the object goes.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> ::tembra::Result<Self> {
                let address = declaration.address()?;
                let porter = format!("{}/{}", address.key.join("/"), address.principal);
                let events = inputs.events.clone();
                declaration.task(move |scope| {
                    Box::pin(async move {
                        events.borrow_mut().push(format!("porter {porter} starts"));
                        scope.signal().cancelled().await;
                        events.borrow_mut().push(format!("porter {porter} stops"));
                        Ok(())
                    })
                })?;
                Ok(Self)
            }
        }
    }
}

/// A worker the host runs while the Orb is installed: an Orb whose only content is its root, which
/// declares the worker, and a resource of the root, the worker's lamp. It exports nothing, so its
/// WIT is an empty world. Native only, as `notebook`. The module holds the bindings of the
/// kernel's `tembra:guest` package, for the lamp, under the name `tembra`, so it names the facade
/// `::tembra`.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:worker@1.0.0")]
pub mod worker {
    use ::tembra::Declaration;
    use std::{cell::RefCell, rc::Rc};

    /// What the host lends the Orb.
    #[inputs]
    pub struct Inputs {
        /// What the worker did, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// Why the worker cannot start, if it cannot: the root then fails with it.
        pub failure: Option<::tembra::Error>,
    }

    /// The worker's lamp, a resource of the Orb's root: the host lights it once the root has
    /// declared the worker, and puts it out when it removes the Orb.
    #[resource]
    pub struct Lamp;

    impl Lamp {
        /// Light the lamp.
        #[open]
        fn open() -> Self {
            super::note("lamp lit".to_owned());
            Lamp
        }

        /// Put the lamp out.
        #[close]
        fn close(self) {
            super::note("lamp out".to_owned());
        }
    }

    /// Declare the worker: a task that runs once the Orb is active and stops when it is
    /// cancelled, as the host closes. Or fail, as the inputs say.
    #[root]
    fn start(inputs: &Inputs, declaration: &Declaration) -> ::tembra::Result<()> {
        if let Some(failure) = &inputs.failure {
            return Err(failure.clone());
        }
        let events = inputs.events.clone();
        declaration.task(move |scope| {
            Box::pin(async move {
                events.borrow_mut().push("run".to_string());
                scope.signal().cancelled().await;
                events.borrow_mut().push("stop".to_string());
                Ok(())
            })
        })
    }
}

/// A meter another Orb consumes: an implementation of `example:meter/gauge@1.0.0`, whose functions
/// are all asynchronous, from the package in `wit/deps/`. Each object keeps its own total.
#[tembra::orb(package = "example:meters@1.0.0", wit = "wit")]
pub mod meters {
    pub mod meter {
        use super::gauge;

        /// The total of one meter, which the kernel keeps for each object.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Total {
            pub total: u64,
        }

        #[service("example:meter/gauge@1.0.0")]
        pub struct Gauge;

        impl Gauge {
            /// Add the reading to this meter's total and return the total after it.
            pub async fn add(reading: gauge::Reading) -> u64 {
                let added = u64::from(reading.amount) * u64::from(reading.times);
                Total::update(|total| total.total += added).total
            }

            /// This meter's total.
            pub async fn total() -> u64 {
                Total::read().total
            }
        }
    }
}

/// Feeds the meter of another Orb, which the host names: the attribute imports its interface, for
/// the `main` object of the provider, and imports it by key, for the object of a key the call
/// chooses. Natively, `metering::orb(..)` takes the provider of each, in `metering::Providers`.
#[tembra::orb(
    package = "example:metering@1.0.0",
    wit = "wit",
    import = "example:meter/gauge@1.0.0",
    import("example:meter/gauge@1.0.0", keyed)
)]
pub mod metering {
    pub mod readings {
        use super::{gauge, gauge_ref};

        /// Feeds meters.
        #[service]
        pub struct Readings;

        impl Readings {
            /// Add each amount, counted once, to the provider's `main` meter, and return the total
            /// it reports after them.
            pub async fn feed(amounts: Vec<u32>) -> u64 {
                for amount in amounts {
                    gauge::add(gauge::Reading { amount, times: 1 }).await;
                }
                gauge::total().await
            }

            /// Add the reading to the meter of `key`, and return the total it reports then, or why
            /// the provider refused the key.
            pub async fn feed_at(key: String, reading: gauge::Reading) -> Result<u64, String> {
                let meter = gauge_ref::get(vec![key]).await?;
                let added = meter.add(reading).await;
                let total = meter.total().await;
                assert_eq!(added, total, "one call at a time feeds a meter");
                Ok(total)
            }

            /// The total of the provider's `main` meter if it answers at once, and 0 otherwise: the
            /// call the method stops awaiting is cancelled.
            pub async fn glance() -> u64 {
                let mut total = std::pin::pin!(gauge::total());
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(match std::future::Future::poll(total.as_mut(), cx) {
                        std::task::Poll::Ready(total) => total,
                        std::task::Poll::Pending => 0,
                    })
                })
                .await
            }
        }
    }
}

/// Swaps the currencies of the ledger's `money`, an interface of the package in `wit/` that holds
/// types only: the attribute imports it for them, and as `cargo tembra build` binds such an import
/// to nothing, `exchange::orb()` takes no provider for it.
#[tembra::orb(
    package = "example:exchange@1.0.0",
    wit = "wit",
    import = "example:ledger/money@1.0.0"
)]
pub mod exchange {
    pub mod desk {
        use super::money;

        /// Swaps currencies.
        #[service]
        pub struct Desk;

        impl Desk {
            /// The other currency of the two.
            pub fn swap(currency: money::Currency) -> money::Currency {
                match currency {
                    money::Currency::Eur => money::Currency::Usd,
                    money::Currency::Usd => money::Currency::Eur,
                }
            }
        }
    }
}

/// Contributes to the hook a gate of the package in `wit/deps/` declares, which the host names:
/// natively, `filter::orb(..)` takes the gate in `filter::Providers`. An Orb needs no service to
/// contribute.
#[tembra::orb(package = "example:filter@1.0.0", wit = "wit", context)]
pub mod filter {
    use std::cell::RefCell;

    std::thread_local! {
        /// What the contribution saw, in order: each value, with the principal of the call that
        /// ran the hook and the object the contribution ran on.
        static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// What the contribution saw since the last call, in order.
    pub fn seen() -> Vec<String> {
        SEEN.with(RefCell::take)
    }

    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:gate/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        let object = context::object();
        SEEN.with(|seen| {
            seen.borrow_mut().push(format!(
                "{} for {} on [{}]@{}",
                input.value,
                context::principal(),
                object.key.join("/"),
                object.principal
            ))
        });
        if input.value < 0 {
            return guard::Admission {
                value: input.value,
                blocked: true,
            };
        }
        input
    }
}

/// Counters for the host, and the board that sums them up: a native Orb as a host writes it by hand
/// against the runtime's API, as the test `counters` does, rewritten with the macro. The host lends
/// it inputs; the Orb's root creates what every object shares, with a resource of the root; the
/// create of `Counter` declares the state, a resource and a task of each object; its `#[local]`
/// method takes a closure; and the board's guard admits one address. Native only, as `notebook`.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:counter@1.0.0")]
pub mod counters {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    use tembra::{Declaration, Resource, core::Address};

    /// What the host lends every service of the Orb.
    #[inputs]
    pub struct Inputs {
        /// What happened, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// The highest count of a counter, resolved from its address.
        pub limit: Box<dyn Fn(&Address) -> u32>,
    }

    /// What every counter of the Orb shares: how many steps they counted in all, and the tape each
    /// count they reach is written on, a resource of the Orb's root.
    pub struct Tally {
        steps: Cell<u32>,
        tape: Resource<RefCell<Vec<String>>>,
    }

    /// Create the tally, once for the Orb, and report it to the host: its tape opens once this
    /// returns, and closes when the host removes the Orb.
    #[root]
    fn tally(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Tally> {
        inputs.events.borrow_mut().push("root".to_string());
        let (opened, closed) = (inputs.events.clone(), inputs.events.clone());
        let tape = declaration.resource(
            "tape",
            1,
            move || {
                opened.borrow_mut().push("open tape".to_string());
                Ok(RefCell::default())
            },
            move |tape: Rc<RefCell<Vec<String>>>, _| {
                Box::pin(async move {
                    let counts = tape.borrow().len();
                    closed
                        .borrow_mut()
                        .push(format!("close tape of {counts} counts"));
                    Ok(())
                })
            },
        )?;
        Ok(Tally {
            steps: Cell::new(0),
            tape,
        })
    }

    pub mod counter {
        use super::{Inputs, Tally};
        use std::rc::Rc;
        use tembra::{Context, Declaration, Resource, State};

        /// A counter, an object per key and principal, which counts up to the limit the host
        /// resolves from its address.
        #[service]
        pub struct Counter {
            /// The object's key and principal, as `a/alice`.
            name: String,
            /// The count, the object's state.
            count: State<u32>,
            /// What the counter shows its count on, a resource of the object.
            display: Resource<String>,
            /// What every counter shares.
            tally: Rc<Tally>,
        }

        impl Counter {
            /// Build a counter, reported to the host, and declare what the object holds: its state,
            /// the count, from zero, which its rule keeps within the counter's limit; its display,
            /// opened once this returns and closed with the object; and a task, which runs while
            /// the object is active.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                let address = declaration.address()?;
                let name = format!("{}/{}", address.key.join("/"), address.principal);
                inputs.events.borrow_mut().push(format!("create {name}"));
                let limit = (inputs.limit)(&address);
                let count = declaration.state(0, 1, move |count: &u32| {
                    if *count <= limit {
                        Ok(())
                    } else {
                        Err(tembra::Error::invalid(
                            "limit",
                            format!("{count} is past the limit of {limit}"),
                        ))
                    }
                })?;
                let (opened, closed) = (inputs.events.clone(), inputs.events.clone());
                let label = name.clone();
                let display = declaration.resource(
                    "display",
                    1,
                    move || {
                        opened.borrow_mut().push(format!("open display of {label}"));
                        Ok(format!("display of {label}"))
                    },
                    move |display, _| {
                        Box::pin(async move {
                            closed.borrow_mut().push(format!("close {display}"));
                            Ok(())
                        })
                    },
                )?;
                let (events, task) = (inputs.events.clone(), name.clone());
                declaration.task(move |scope| {
                    Box::pin(async move {
                        events.borrow_mut().push(format!("task of {task} starts"));
                        scope.signal().cancelled().await;
                        events.borrow_mut().push(format!("task of {task} stops"));
                        Ok(())
                    })
                })?;
                Ok(Self {
                    name,
                    count,
                    display,
                    tally: declaration.orb::<Tally>()?,
                })
            }

            /// Count a step and return the count, or fail past the counter's limit.
            pub fn step(&self, context: &Context) -> tembra::Result<u32> {
                self.step_by(context, 1)
            }

            /// Count `steps` steps at once and return the count, or fail past the counter's limit,
            /// counting none.
            pub fn step_by(&self, context: &Context, steps: u32) -> tembra::Result<u32> {
                let count = *self.count.update(context, |count| Ok(count + steps))?;
                self.tally.steps.set(self.tally.steps.get() + steps);
                self.tally
                    .tape
                    .value()?
                    .borrow_mut()
                    .push(format!("{} at {count}", self.name));
                Ok(count)
            }

            /// The count.
            pub fn count(&self) -> tembra::Result<u32> {
                Ok(*self.count.value()?)
            }

            /// The count, as the counter's display shows it.
            pub fn display(&self) -> tembra::Result<String> {
                Ok(format!(
                    "{}: {}",
                    self.display.value()?,
                    self.count.value()?
                ))
            }

            /// Set the count to what the host's operation makes of it, and return it, or fail past
            /// the counter's limit: a closure, which only exists natively and has no serde, so the
            /// method is `#[local]` and the WIT leaves it out.
            #[local]
            pub fn apply(
                &self,
                context: &Context,
                operation: Rc<dyn Fn(u32) -> u32>,
            ) -> tembra::Result<u32> {
                Ok(*self.count.update(context, |count| Ok(operation(*count)))?)
            }
        }
    }

    pub mod board {
        use super::{Inputs, Tally};
        use std::rc::Rc;
        use tembra::Declaration;

        /// The board of the counters: one object, of key `main` and principal `host`, which its
        /// factory guards.
        #[service(key = main, principal = "host")]
        pub struct Board {
            tally: Rc<Tally>,
        }

        impl Board {
            /// Build the board on the tally the root created, reported to the host: an address the
            /// guard refuses never gets here.
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                inputs.events.borrow_mut().push("create board".to_string());
                Ok(Self {
                    tally: declaration.orb::<Tally>()?,
                })
            }

            /// How many steps the counters counted in all.
            pub fn steps(&self) -> u32 {
                self.tally.steps.get()
            }

            /// What the tape holds: each count the counters reached, in order.
            pub fn tape(&self) -> tembra::Result<Vec<String>> {
                Ok(self.tally.tape.value()?.borrow().clone())
            }
        }
    }
}

/// An abacus whose beads are a resource of the Orb's root, at version 1: a native host's reload
/// keeps them, with what was slid on them, while their version stays the same. `restrung` is the
/// same Orb a revision later, whose beads are at version 2. The beads note what they go through in
/// the journal and in the host's log, through the `context` of their opener and closer, which
/// names the Orb also when a reload opens or closes them.
#[tembra::orb(package = "example:abacus@1.0.0", context)]
pub mod abacus {
    /// The beads, a resource of the Orb's root, at the default version, 1.
    #[resource]
    pub struct Beads {
        slid: i64,
    }

    impl Beads {
        /// Set the beads at zero, as the host opens them.
        #[open]
        fn open() -> Self {
            super::note("beads 1 open".to_owned());
            context::log(context::Level::Info, "beads 1 open");
            Beads { slid: 0 }
        }

        /// Note how far the beads were slid, as the host closes them.
        #[close]
        fn close(self) {
            let event = format!("beads 1 closed at {}", self.slid);
            context::log(context::Level::Info, &event);
            super::note(event);
        }
    }

    pub mod rod {
        use super::Beads;

        /// Slides the beads.
        #[service]
        pub struct Rod;

        impl Rod {
            /// Slide the beads by `by` and return how far they were slid in all.
            pub fn slide(by: i64) -> i64 {
                Beads::with(|beads| {
                    beads.slid += by;
                    beads.slid
                })
            }
        }
    }
}

/// The abacus a revision later, restrung: its beads, a resource of the Orb's root, are at version
/// 2, so a native host's reload from `abacus` closes the beads slid there and opens these. Its
/// package is at 2.0.0, as two modules of one package in a crate would each remove the other's
/// contract.
#[tembra::orb(package = "example:abacus@2.0.0", context)]
pub mod restrung {
    /// The beads, a resource of the Orb's root, at version 2.
    #[resource(version = 2)]
    pub struct Beads {
        slid: i64,
    }

    impl Beads {
        /// Set the beads at zero, as the host opens them, in the host's log too.
        #[open]
        fn open() -> Self {
            super::note("beads 2 open".to_owned());
            context::log(context::Level::Info, "beads 2 open");
            Beads { slid: 0 }
        }

        /// Note how far the beads were slid, as the host closes them, in the host's log too.
        #[close]
        fn close(self) {
            let event = format!("beads 2 closed at {}", self.slid);
            context::log(context::Level::Info, &event);
            super::note(event);
        }
    }

    pub mod rod {
        use super::Beads;

        /// Slides the beads.
        #[service]
        pub struct Rod;

        impl Rod {
            /// Slide the beads by `by` and return how far they were slid in all.
            pub fn slide(by: i64) -> i64 {
                Beads::with(|beads| {
                    beads.slid += by;
                    beads.slid
                })
            }
        }
    }
}

/// A cistern whose tank and gauge are resources of the Orb's root, both at version 1, the gauge
/// depending on the tank, whose level its `#[open]` reads. `refilled` is the same Orb a revision
/// later, whose tank is at version 2 and whose gauge stays at version 1.
#[tembra::orb(package = "example:cistern@1.0.0")]
pub mod cistern {
    /// The tank, at the default version, 1.
    #[resource]
    pub struct Tank {
        level: i64,
    }

    impl Tank {
        #[open]
        fn open() -> Self {
            super::note("tank 1 open".to_owned());
            Tank { level: 1 }
        }

        #[close]
        fn close(self) {
            super::note("tank 1 closed".to_owned());
        }
    }

    /// The gauge, at version 1: the tank's level when it opened.
    #[resource(depends = [Tank])]
    pub struct Gauge {
        read: i64,
    }

    impl Gauge {
        #[open]
        fn open() -> Self {
            let read = Tank::with(|tank| tank.level);
            super::note(format!("gauge 1 open at {read}"));
            Gauge { read }
        }

        #[close]
        fn close(self) {
            super::note(format!("gauge 1 closed at {}", self.read));
        }
    }

    pub mod meter {
        use super::Gauge;

        /// Reads the gauge.
        #[service]
        pub struct Meter;

        impl Meter {
            /// The tank's level the gauge read when it opened.
            pub fn read() -> i64 {
                Gauge::with(|gauge| gauge.read)
            }
        }
    }
}

/// The cistern a revision later, refilled: its tank is at version 2, and its gauge, which depends
/// on the tank, stays at version 1. A native host's reload from `cistern` opens the new tank and
/// the new gauge too, since the kernel keeps a resource only when its dependencies are the ones it
/// opened after. Its package is at 2.0.0, as `restrung`'s is.
#[tembra::orb(package = "example:cistern@2.0.0")]
pub mod refilled {
    /// The tank, at version 2.
    #[resource(version = 2)]
    pub struct Tank {
        level: i64,
    }

    impl Tank {
        #[open]
        fn open() -> Self {
            super::note("tank 2 open".to_owned());
            Tank { level: 2 }
        }

        #[close]
        fn close(self) {
            super::note("tank 2 closed".to_owned());
        }
    }

    /// The gauge, still at version 1: the tank's level when it opened.
    #[resource(depends = [Tank])]
    pub struct Gauge {
        read: i64,
    }

    impl Gauge {
        #[open]
        fn open() -> Self {
            let read = Tank::with(|tank| tank.level);
            super::note(format!("gauge 1 open at {read}"));
            Gauge { read }
        }

        #[close]
        fn close(self) {
            super::note(format!("gauge 1 closed at {}", self.read));
        }
    }

    pub mod meter {
        use super::Gauge;

        /// Reads the gauge.
        #[service]
        pub struct Meter;

        impl Meter {
            /// The tank's level the gauge read when it opened.
            pub fn read() -> i64 {
                Gauge::with(|gauge| gauge.read)
            }
        }
    }
}

/// The abacus a revision later, in a new frame: its package is at 1.1.0, and its beads, a resource
/// of the Orb's root, stay at version 1, so a native host's reload from `abacus` keeps the beads
/// slid there, as their version alone decides. These beads are another Rust type than `abacus`'s,
/// as the struct of any other module is, even with the same fields, so this code cannot reach the
/// value kept for it, and each call of its rod answers `invalid/rust_type`.
#[tembra::orb(package = "example:abacus@1.1.0")]
pub mod reframed {
    /// The beads, a resource of the Orb's root, at the default version, 1, as `abacus`'s are.
    #[resource]
    pub struct Beads {
        slid: i64,
    }

    impl Beads {
        /// Set the beads at zero, as the host opens them.
        #[open]
        fn open() -> Self {
            super::note("reframed beads open".to_owned());
            Beads { slid: 0 }
        }

        /// Note how far the beads were slid, as the host closes them.
        #[close]
        fn close(self) {
            super::note(format!("reframed beads closed at {}", self.slid));
        }
    }

    pub mod rod {
        use super::Beads;

        /// Slides the beads.
        #[service]
        pub struct Rod;

        impl Rod {
            /// Slide the beads by `by` and return how far they were slid in all.
            pub fn slide(by: i64) -> i64 {
                Beads::with(|beads| {
                    beads.slid += by;
                    beads.slid
                })
            }
        }
    }
}

/// Rolls a die with the host's secure random source, through a WASI import, which the native side
/// does not cover yet: the module compiles where the runtime is linked, and its `orb()` answers an
/// error naming the import.
#[tembra::orb(package = "example:dice@1.0.0", import = "wasi:random/random@0.3.0")]
pub mod dice {
    pub mod die {
        use super::random;

        /// The die.
        #[service]
        pub struct Die;

        impl Die {
            /// A roll, from 1 to 6.
            pub fn roll() -> u64 {
                random::get_random_u64() % 6 + 1
            }
        }
    }
}

/// Pass values through a synchronous WIT gate. The native imported call is awaitable; the
/// compiled call retains the synchronous bindings of the unchanged provider contract.
#[tembra::orb(
    package = "example:turnstile@1.0.0",
    wit = "wit",
    import = "example:gate/admissions@1.0.0"
)]
pub mod turnstile {
    pub mod entrance {
        use super::admissions;

        /// The entrance.
        #[service]
        pub struct Entrance;

        impl Entrance {
            /// Whether the gate lets the value through.
            pub async fn pass(value: i32) -> bool {
                #[cfg(target_arch = "wasm32")]
                let admitted = admissions::admit(value);
                #[cfg(not(target_arch = "wasm32"))]
                let admitted = admissions::admit(value).await;
                !admitted.blocked
            }
        }
    }
}

/// Declares a hook, whose `run` is synchronous: a native method could not wait for the hook's
/// contributions, so the declaration works in a component build only, and the module's `orb()`
/// answers an error naming it.
#[tembra::orb(package = "example:checkpoint@1.0.0")]
pub mod checkpoint {
    /// The hook: a waterfall, each contributor may change the inspection, in plan order.
    #[hook(composition = waterfall, input = Inspection)]
    pub struct Inspect;

    /// One inspection: the value, and whether a contributor stopped it.
    pub struct Inspection {
        pub value: i32,
        pub stopped: bool,
    }

    pub mod inspector {
        use super::{Inspect, Inspection};

        /// The inspector.
        #[service]
        pub struct Inspector;

        impl Inspector {
            /// Run the hook over a fresh inspection of the value.
            pub fn inspect(value: i32) -> Inspection {
                Inspect::run(Inspection {
                    value,
                    stopped: false,
                })
            }
        }
    }
}

/// Lends sessions in a record beside their count: a record holding a resource is the author's,
/// whose resource a component's bridge wraps into the bindings' handle, while the native boundary
/// carries a resource only beside the data, so the module's `orb()` answers an error naming the
/// record's field, where a tuple would carry the resource natively too.
#[tembra::orb(package = "example:desk@1.0.0")]
pub mod desk {
    pub mod sessions {
        /// One session.
        #[wit_resource]
        pub struct Session {
            n: u32,
        }

        impl Session {
            /// A session numbered `n`.
            #[constructor]
            pub fn new(n: u32) -> Session {
                Session { n }
            }

            /// The session's number.
            pub fn number(&self) -> u32 {
                self.n
            }
        }

        /// A session and its count.
        pub struct Lease {
            pub session: Session,
            pub count: u32,
        }

        /// The desk.
        #[service]
        pub struct Sessions;

        impl Sessions {
            /// Lend a session numbered `n`, counted once.
            pub fn lend(n: u32) -> Lease {
                Lease {
                    session: Session::new(n),
                    count: 1,
                }
            }
        }
    }
}

/// A trail whose services and root `orb()` reaches wherever the module puts them: the Orb's root
/// two private modules below the Orb module and the walker in a module below it, and the guide,
/// whose method shares its name with a method of the `Rc` each guide object is kept in, which
/// `orb()` calls as the guide's. Native only, as `notebook`: the guide's create and the root take
/// inputs.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:trail@1.0.0")]
pub mod trail {
    /// What the host lends the Orb.
    #[inputs]
    pub struct Inputs {
        /// The name of the trail.
        pub name: String,
    }

    pub mod a {
        mod b {
            use tembra::Declaration;

            /// The map of the trail, which every object of the Orb shares.
            pub struct Map {
                /// How many legs the trail has.
                pub legs: u32,
            }

            /// Map the trail, a leg per word of its name: the Orb's root, which the Orb module
            /// cannot name either.
            #[root]
            pub(crate) fn map(
                inputs: &super::super::Inputs,
                _declaration: &Declaration,
            ) -> tembra::Result<Map> {
                let legs = inputs.name.split_whitespace().count() as u32;
                Ok(Map { legs })
            }

            pub mod walker {
                use super::Map;
                use tembra::Context;

                /// A walker, which the Orb module cannot name: the module `b` is private to `a`.
                #[service]
                pub struct Walker;

                impl Walker {
                    /// How many steps a walk takes.
                    pub fn steps() -> u32 {
                        1
                    }

                    /// How many legs the trail has, as the Orb's root mapped them.
                    pub fn legs(context: &Context) -> u32 {
                        context.orb::<Map>().map_or(0, |map| map.legs)
                    }
                }
            }
        }
    }

    pub mod guide {
        use super::Inputs;
        use tembra::Declaration;

        /// A guide of the trail the host names.
        #[service]
        pub struct Guide {
            name: String,
        }

        impl Guide {
            /// Build a guide of the trail.
            #[create]
            fn create(inputs: &Inputs, _declaration: &Declaration) -> tembra::Result<Self> {
                Ok(Self {
                    name: inputs.name.clone(),
                })
            }

            /// A copy of the trail's name, named as `Rc::clone` is.
            #[allow(clippy::should_implement_trait)]
            pub fn clone(&self) -> String {
                format!("a copy of {}", self.name)
            }
        }
    }
}

/// A sketch whose state holds what `serde` reads through its buffer: an internally tagged enum
/// and a flattened struct, each holding an `f64`, which natively read back as in a component.
#[tembra::orb(package = "example:sketch@1.0.0")]
pub mod sketch {
    /// The shape drawn, tagged by `kind` inside its own object. The derives of `serde` are the
    /// author's, which its attribute follows.
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "kind")]
    pub enum Shape {
        #[default]
        Dot,
        Circle {
            radius: f64,
        },
    }

    /// How large the sketch is drawn, whose fields sit beside the shape's in the state's JSON.
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    pub struct Size {
        pub scale: f64,
    }

    pub mod pad {
        use super::{Shape, Size};

        /// The sketch an object keeps.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Sketch {
            pub shape: Shape,
            #[serde(flatten)]
            pub size: Size,
        }

        /// Draws one sketch per object.
        #[service]
        pub struct Pad;

        impl Pad {
            /// Draw a circle of `radius` at `scale`, and return the radius and the scale the state
            /// reads back.
            pub fn circle(radius: f64, scale: f64) -> (f64, f64) {
                Sketch::update(|sketch| {
                    sketch.shape = Shape::Circle { radius };
                    sketch.size.scale = scale;
                });
                let sketch = Sketch::read();
                match sketch.shape {
                    Shape::Circle { radius } => (radius, sketch.size.scale),
                    Shape::Dot => (0.0, sketch.size.scale),
                }
            }
        }
    }
}

/// A service in a private module, `tokens`, whose state sits in a module below it, `kept`, and
/// whose WIT resource, with a constructor and a static function, sits in another, `tags`: each
/// belongs to the service of the nearest service's module above it, and the resource's module,
/// below the service's, names its client. The client and the handle are as private as the module,
/// so the Orb re-exports them where the runtime is linked, as a component build has none.
#[tembra::orb(package = "example:hollow@1.0.0")]
pub mod hollow {
    #[cfg(not(target_arch = "wasm32"))]
    pub use tokens::{TokensClient, tags::TokenHandle};

    mod tokens {
        pub mod kept {
            /// How many tokens an object of the service issued, which the kernel keeps for it.
            #[state]
            #[derive(Default, serde::Serialize, serde::Deserialize)]
            pub struct Issued {
                pub count: u32,
            }
        }

        pub mod tags {
            /// A numbered token, which the service and its constructor issue.
            #[wit_resource]
            pub struct Token {
                n: u32,
            }

            impl Token {
                /// A token numbered `n`, counted in the state of the object issuing it.
                #[constructor]
                pub fn new(n: u32) -> Token {
                    super::kept::Issued::update(|issued| issued.count += 1);
                    Token { n }
                }

                /// A token numbered ten times `n`: a static function.
                pub fn tenfold(n: u32) -> Token {
                    Token::new(n * 10)
                }

                /// The token's number.
                pub fn number(&self) -> u32 {
                    self.n
                }
            }
        }

        /// Hands out tokens.
        #[service]
        pub struct Tokens;

        impl Tokens {
            /// A token numbered `n`.
            pub fn open(n: u32) -> tags::Token {
                tags::Token::new(n)
            }

            /// How many tokens this object issued.
            pub fn issued() -> u32 {
                kept::Issued::read().count
            }
        }
    }
}

/// Bollards for the boats a create builds, with what the host lends the Orb: the argument of
/// `pier::orb(..)`. Each `Boat` object is built by its create from the host's inputs and owns a
/// bollard, a resource of the service's module, which the factory declares after the create ran
/// and the host opens with the object; a method borrowing `&self` reaches both.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:pier@1.0.0")]
pub mod pier {
    /// What the host lends every service of the Orb.
    #[inputs]
    pub struct Inputs {
        /// The port the boats are named after.
        pub port: String,
    }

    pub mod boat {
        use super::Inputs;
        use tembra::Declaration;

        /// A boat, named by its create after the port and its key.
        #[service]
        pub struct Boat {
            name: String,
        }

        impl Boat {
            #[create]
            fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
                Ok(Boat {
                    name: format!("{} {}", inputs.port, declaration.address()?.key.join("/")),
                })
            }

            /// Tie `ropes` more to this boat's bollard, and answer the boat's name with how many
            /// ropes its bollard holds.
            pub fn tie(&self, ropes: u32) -> String {
                let ropes = Bollard::with(|bollard| {
                    bollard.ropes += ropes;
                    bollard.ropes
                });
                format!("{}: {ropes} ropes", self.name)
            }
        }

        /// The bollard of one boat, holding its ropes.
        #[resource]
        pub struct Bollard {
            pub ropes: u32,
        }

        impl Bollard {
            #[open]
            pub fn open() -> Bollard {
                Bollard { ropes: 0 }
            }
        }
    }
}

pub mod codec;
