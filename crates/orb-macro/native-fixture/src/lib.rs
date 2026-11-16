//! An Orb crate used natively. The module `calculator` is one `cargo tembra build` would compile
//! into a component; the tests of this crate are a native host instead, which installs
//! `calculator::orb()` and calls its services locally, with Rust values, and portably, with the
//! boundary's JSON. The module `notebook` takes host values, which only a native host can lend,
//! `library` shares its root's value among its objects, and `worker` only runs its root. The
//! manifest names every Orb of the crate `calculator` at API version 2, which `orb()` reports.

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
}

/// Notes kept for the host, with what the host lends the Orb: the argument of `notebook::orb(..)`.
/// Native only, as host values and the objects a create builds are.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:notes@1.0.0")]
pub mod notebook {
    use std::{cell::RefCell, rc::Rc};
    use tembra::{Declaration, Resource, core::Address};

    /// What the host lends every service of the Orb.
    #[inputs]
    pub struct Inputs {
        /// What happened, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// The title of an object's notes, resolved from its address.
        pub title: Box<dyn Fn(&Address) -> String>,
    }

    /// The notes of one object, which its create builds once per key and principal.
    #[service]
    pub struct Notes {
        title: String,
        notes: RefCell<Vec<String>>,
        journal: Resource<String>,
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
}

/// Shelves of books, with what every shelf shares: the catalogue the Orb's root creates once, which
/// each shelf's create and methods read; and porters, objects without methods that run a task.
/// Native only, as `notebook`.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:library@1.0.0")]
pub mod library {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    use tembra::Declaration;

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
    fn catalogue(inputs: &Inputs, _declaration: &Declaration) -> tembra::Result<Catalogue> {
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

    impl Shelf {
        /// Build a shelf on the catalogue the root created, and report how many books the
        /// library held then.
        #[create]
        fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
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

        /// Shelve a book, and return how many books the library holds, across its shelves.
        pub fn shelve(&self) -> u32 {
            let books = self.catalogue.books.get() + 1;
            self.catalogue.books.set(books);
            books
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
        fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
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
/// declares the worker. It exports nothing, so its WIT is an empty world. Native only, as
/// `notebook`.
#[cfg(not(target_arch = "wasm32"))]
#[tembra::orb(package = "example:worker@1.0.0")]
pub mod worker {
    use std::{cell::RefCell, rc::Rc};
    use tembra::Declaration;

    /// What the host lends the Orb.
    #[inputs]
    pub struct Inputs {
        /// What the worker did, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// Why the worker cannot start, if it cannot: the root then fails with it.
        pub failure: Option<tembra::Error>,
    }

    /// Declare the worker: a task that runs once the Orb is active and stops when it is
    /// cancelled, as the host closes. Or fail, as the inputs say.
    #[root]
    fn start(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<()> {
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
