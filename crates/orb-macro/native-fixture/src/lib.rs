//! An Orb crate used natively. The module `calculator` is one `cargo tembra build` would compile
//! into a component; the tests of this crate are a native host instead, which installs
//! `calculator::orb()` and calls its services locally, with Rust values, and portably, with the
//! boundary's JSON. The module `notebook` takes host values, which only a native host can lend,
//! and `library` shares its root's value among its objects. The manifest names every Orb of the
//! crate `calculator` at API version 2, which `orb()` reports.

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

        impl Memo {
            /// The text, as given.
            pub fn echo(text: String) -> String {
                text
            }
        }
    }
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
/// each shelf's create and methods read. Native only, as `notebook`.
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
}
