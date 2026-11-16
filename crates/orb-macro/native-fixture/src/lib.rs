//! An Orb crate used natively. The module `calculator` is one `cargo tembra build` would compile
//! into a component; the tests of this crate are a native host instead, which installs
//! `calculator::orb()` and calls its services locally, with Rust values, and portably, with the
//! boundary's JSON, and whose methods read their call through the kernel's `context`, their
//! object's state and the resources of the Orb's root, as a component's do. The module `notebook`
//! takes host values, which only a native host can lend, `library` shares its root's value among
//! its objects, and `worker` only runs its root and a resource of it. The manifest names every Orb
//! of the crate `calculator` at API version 2, which `orb()` reports.

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

#[tembra::orb(package = "example:calculator@1.0.0", context)]
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

    /// A point of the plotter's grid: 64-bit coordinates cross the boundary as exact numbers.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Point {
        pub x: i64,
        pub y: i64,
    }

    /// Where the pen moves.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Heading {
        North,
        East,
        South,
        West,
    }

    /// What the plotter draws: a case without a value, one with a value, and one with named fields,
    /// which carries a record of its own, so the macro keeps the enum the author's.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Shape {
        Dot,
        Square(u32),
        Rect { width: u32, height: u32 },
    }

    /// How a shape is drawn.
    #[flags]
    #[derive(PartialEq)]
    pub struct Pen {
        pub filled: bool,
        pub dashed_line: bool,
    }

    /// The moves of a walk, in a private module: their JSON is written there, and the Orb module's
    /// services name them by path.
    mod moves {
        /// One move of a walk: no case has named fields, so the bindings generate the enum.
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

    /// Plots on a grid, with records, enums, variants, flags, options, results and 64-bit
    /// integers.
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

        /// The stroke with its shape scaled by the factor and each flag of its pen flipped.
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

        /// Which of the points lie on a heading's axis from the origin, in order, and how many do
        /// not: a list of options and a result without values.
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

    /// Where a walker stands, which the kernel keeps for each walker object: a method reads and
    /// updates it through the kernel, natively as in a component. The kernel keeps it as the JSON
    /// serde writes, the same in both, which is that of the runtime fixture's walker: `{"at": {"x":
    /// 0, "y": 0}, "facing": "North", "moves": 0}` to start with.
    #[state(service = Walker)]
    #[derive(Debug, PartialEq)]
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
        /// The rule of the state: a walker stays on the grid, at most 1000 steps from the origin
        /// along each axis, or as many as `set_grid` says. The kernel keeps where it stood when an
        /// update breaks it, and the running code when a reload inherits a walker it breaks.
        #[validate]
        pub fn on_grid(&self) -> Result<(), String> {
            let steps = super::GRID.with(std::cell::Cell::get);
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
        /// Make the moves from where the walker stands, heading as it faces, and return where it
        /// ends, or why the kernel keeps it where it stood.
        pub fn walk(path: Vec<moves::Move>) -> Result<Position, String> {
            Position::update(|position| {
                for step in &path {
                    match *step {
                        moves::Move::Turn(heading) => position.facing = heading,
                        moves::Move::Step(count) => {
                            position.at =
                                Plotter::step(position.at, Some(position.facing), u64::from(count));
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

        /// Cancel the call, then make the moves: the kernel refuses the value of a cancelled call
        /// and keeps where the walker stood, which `update` answers with, as in a component.
        pub fn stumble(path: Vec<moves::Move>) -> Result<Position, String> {
            context::cancel();
            Self::walk(path)
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

        /// Wait until the caller cancels the call, then cancel it from inside: a cancelled call's
        /// cancel changes nothing, so its root and the root's other calls go on.
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
    /// of the module's only service, which a caller constructs or opens, adds to through its handle,
    /// and drops.
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
}

/// Notes kept for the host, with what the host lends the Orb: the argument of `notebook::orb(..)`.
/// Native only, as host values, the objects a create builds, a method's `&Context` and a `#[local]`
/// method's closure are. The crate's `archive` feature adds an input, the Orb's root, a method of
/// `Notes` and the sub-module `shelf` with its service, under `#[cfg]`s the macro evaluates.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:notes@1.0.0")]
pub mod notebook {
    use std::{cell::RefCell, rc::Rc};
    use tembra::{Context, Declaration, Resource, core::Address};

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
    fn share(inputs: &Inputs, _: &Declaration) -> tembra::Result<Archive> {
        Ok(Archive(inputs.archive.clone()))
    }

    /// The notes of one object, which its create builds once per key and principal.
    #[service]
    pub struct Notes {
        title: String,
        notes: RefCell<Vec<String>>,
        journal: Resource<String>,
        #[cfg(feature = "archive")]
        archive: Rc<RefCell<Vec<String>>>,
    }

    impl Notes {
        /// Build an object: its title, which the host resolves from its address, and what it
        /// declares, each reported to the host: its state, the object's own label; its journal, a
        /// resource opened once this returns and closed with the object; and a task, run once the
        /// object is active.
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
                archive: declaration.orb::<Archive>()?.0.clone(),
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

    /// What a call carries besides its arguments: a method that takes `&Context` reads the call's
    /// context, which a native host passes and the WIT leaves out.
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
    use ::tembra::{Context, Declaration};
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

    /// One shelf of the library, an object per key and principal.
    #[service]
    pub struct Shelf {
        catalogue: Rc<Catalogue>,
    }

    /// How many books one shelf holds, which the kernel keeps for each shelf.
    #[state(service = Shelf)]
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

        /// How many books the library holds, read from the catalogue through the call's context,
        /// as a method that keeps nothing of the root reads it.
        pub fn books(context: &Context) -> u32 {
            context
                .orb::<Catalogue>()
                .map_or(0, |catalogue| catalogue.books.get())
        }
    }

    /// A porter who tidies the library while someone keeps one at hand: its objects have no
    /// method, only the task their create declares.
    #[service]
    pub struct Porter;

    impl Porter {
        /// Declare the porter's round: a task reported when it starts, once the object is active,
        /// and when it is cancelled, as the object goes.
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
