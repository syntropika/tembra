//! One Orb that builds natively and as a component, so that a test compares the two:
//! `executor-wasm/tests/orb_macro_parity.rs` installs `parity::orb(..)` in one host and the
//! component `just test-orb-parity` builds from this crate in another, makes the same calls on both
//! and requires the same answers, as the macro promises that a module behaves alike either way.
//!
//! Each module of the Orb serves one feature of that test's list. `imports` turns the dials of
//! another Orb, through the plain import and the view of the import by key; `cancellation` stops
//! awaiting a call of the plain import; `ledger` keeps a state, and `sketching` one holding an
//! internally tagged enum; `tallies` issues WIT resources, and `vault`, a private service's module,
//! a WIT resource and a state each in a module below it; `checks` takes and returns what the
//! boundary checks; `names` names the types of `checks` through a `use` with `as` and an alias;
//! and `cellar` holds root resources, one depending on the other. A new scenario is a module here,
//! or a function of one, and a row of the test's list. The Orb holds nothing a native host
//! refuses by design, a hook declaration, a synchronous import or a WASI import, and nothing
//! native only.

#[tembra::orb(
    package = "example:parity@1.0.0",
    wit = "consumed",
    import = "example:dials/dial@1.0.0",
    import("example:dials/dial@1.0.0", keyed)
)]
pub mod parity {
    /// Turns the dials of another Orb: its `main` dial through the plain import, which the Orb's
    /// root holds, and the dial of a key through the view of the import by key, which a call
    /// acquires and drops.
    pub mod imports {
        use super::{dial, dial_ref};

        #[service]
        pub struct Dialing;

        impl Dialing {
            /// Turn the provider's `main` dial by each amount and return where it stands then.
            pub async fn turn_main(amounts: Vec<u32>) -> u64 {
                for amount in amounts {
                    dial::turn(amount).await;
                }
                dial::position().await
            }

            /// Turn the dial of `key` by `by` through a view, which the call drops as it ends, and
            /// return where the dial stands then, or why the provider refused the key.
            pub async fn turn_at(key: String, by: u32) -> Result<u64, String> {
                let dial = dial_ref::get(vec![key]).await?;
                dial.turn(by).await;
                Ok(dial.position().await)
            }

            /// Turn the dial of `key` by `by` through two views and the dial of `other` through a
            /// third, and return what each turn answered and where both dials stand then.
            pub async fn turn_both(
                key: String,
                other: String,
                by: u32,
            ) -> Result<Vec<u64>, String> {
                let first = dial_ref::get(vec![key.clone()]).await?;
                let again = dial_ref::get(vec![key]).await?;
                let second = dial_ref::get(vec![other]).await?;
                Ok(vec![
                    first.turn(by).await,
                    again.turn(by).await,
                    second.turn(by).await,
                    first.position().await,
                    second.position().await,
                ])
            }
        }
    }

    /// Calls the plain import's `hold`, which answers once its call is cancelled: a method that
    /// stops awaiting the call cancels it and ends once it has ended, and a method its caller
    /// cancels cancels the call it awaits.
    pub mod cancellation {
        use super::dial;

        #[service]
        pub struct Glancing;

        impl Glancing {
            /// Where the provider's `main` dial stands if `hold` answers at once, and 0 otherwise:
            /// the method stops awaiting the call after one poll.
            pub async fn glance() -> u64 {
                let mut held = core::pin::pin!(dial::hold());
                core::future::poll_fn(|cx| {
                    core::task::Poll::Ready(match core::future::Future::poll(held.as_mut(), cx) {
                        core::task::Poll::Ready(position) => position,
                        core::task::Poll::Pending => 0,
                    })
                })
                .await
            }

            /// Where the provider's `main` dial stands, once `hold` answers.
            pub async fn wait() -> u64 {
                dial::hold().await
            }
        }
    }

    /// Keeps a book in a state, which the kernel stores as the JSON `serde` writes for it: a list
    /// of records, an option of a variant with a case of each kind, a 64-bit balance and a record of
    /// `bool` fields.
    pub mod ledger {
        /// One deposit.
        #[derive(serde::Serialize, serde::Deserialize)]
        pub struct Entry {
            pub label: String,
            pub amount: u64,
        }

        /// What moved the balance last.
        #[derive(serde::Serialize, serde::Deserialize)]
        pub enum Movement {
            Opened,
            Deposit(u64),
            Transfer { to: String, amount: u64 },
        }

        /// What the book went through. The state's `Default` starts it from its own.
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Marks {
            pub audited: bool,
            pub disputed: bool,
        }

        /// The book of one ledger object, which the kernel keeps.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Book {
            pub entries: Vec<Entry>,
            pub last: Option<Movement>,
            pub balance: i64,
            pub marks: Marks,
        }

        #[service]
        pub struct Ledger;

        impl Ledger {
            /// Open the book anew: no entry, a zero balance and no mark.
            pub fn open() -> Book {
                Book::update(|book| {
                    *book = Book {
                        entries: Vec::new(),
                        last: Some(Movement::Opened),
                        balance: 0,
                        marks: Marks::default(),
                    };
                })
            }

            /// Record a deposit and return the balance after it.
            pub fn deposit(label: String, amount: u64) -> i64 {
                Book::update(|book| {
                    book.balance += amount as i64;
                    book.entries.push(Entry { label, amount });
                    book.last = Some(Movement::Deposit(amount));
                })
                .balance
            }

            /// Move `amount` to `to` and return the balance after it, or why the balance is short.
            pub fn transfer(to: String, amount: u64) -> Result<i64, String> {
                if Book::read().balance < amount as i64 {
                    return Err(format!("the balance is short of {amount}"));
                }
                Ok(Book::update(|book| {
                    book.balance -= amount as i64;
                    book.last = Some(Movement::Transfer { to, amount });
                })
                .balance)
            }

            /// Set the marks and return the ones set before.
            pub fn mark(marks: Marks) -> Marks {
                let before = Book::read().marks;
                Book::update(|book| book.marks = marks);
                before
            }

            /// The book as the kernel keeps it.
            pub fn kept() -> Book {
                Book::read()
            }
        }
    }

    /// Keeps a sketch in a state that stays out of the WIT, whose shape is an enum `serde` tags
    /// inside its own object, `#[serde(tag = "kind")]`: the serde derives are the author's, so the
    /// helper attribute follows them.
    pub mod sketching {
        /// The shape drawn, tagged by `kind`.
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "kind")]
        pub enum Shape {
            #[default]
            Dot,
            Circle {
                radius: u32,
            },
            Square {
                side: u32,
            },
        }

        /// The sketch of one sketching object, which the kernel keeps.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Sketch {
            pub shape: Shape,
            pub strokes: u32,
        }

        #[service]
        pub struct Sketching;

        impl Sketching {
            /// Draw a circle of `radius` and return the radius the state reads back.
            pub fn circle(radius: u32) -> u32 {
                Sketch::update(|sketch| {
                    sketch.shape = Shape::Circle { radius };
                    sketch.strokes += 1;
                });
                match Sketch::read().shape {
                    Shape::Circle { radius } => radius,
                    Shape::Dot | Shape::Square { .. } => 0,
                }
            }

            /// Draw a square of `side` and return the side the state reads back.
            pub fn square(side: u32) -> u32 {
                Sketch::update(|sketch| {
                    sketch.shape = Shape::Square { side };
                    sketch.strokes += 1;
                });
                match Sketch::read().shape {
                    Shape::Square { side } => side,
                    Shape::Dot | Shape::Circle { .. } => 0,
                }
            }
        }
    }

    /// A service in a private module, whose state sits in a module below it, `kept`, and whose WIT
    /// resource, with a constructor that counts in that state, in another, `tags`: each belongs to
    /// the service of the nearest service's module above it.
    mod vault {
        pub mod kept {
            /// How many tokens an object of the vault issued.
            #[state]
            #[derive(Default, serde::Serialize, serde::Deserialize)]
            pub struct Issued {
                pub count: u32,
            }
        }

        pub mod tags {
            /// A numbered token.
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

                /// The token's number.
                pub fn number(&self) -> u32 {
                    self.n
                }
            }
        }

        #[service]
        pub struct Vault;

        impl Vault {
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

    /// Issues WIT resources, which its callers own: a caller constructs a tally or receives some,
    /// alone or nested, calls their methods through the handles and drops them, which drops the
    /// tallies.
    pub mod tallies {
        use std::cell::Cell;

        std::thread_local! {
            /// How many tallies were dropped on this thread: a test's host runs on its own.
            static DROPPED: Cell<u32> = const { Cell::new(0) };
        }

        /// A running total its caller owns.
        #[wit_resource]
        pub struct Tally {
            total: Cell<u32>,
        }

        impl Tally {
            /// A tally starting at `start`: the resource's constructor.
            #[constructor]
            pub fn new(start: u32) -> Tally {
                Tally {
                    total: Cell::new(start),
                }
            }

            /// Add to the total and return it.
            pub fn add(&self, by: u32) -> u32 {
                self.total.set(self.total.get() + by);
                self.total.get()
            }

            /// The total: an async method.
            pub async fn total(&self) -> u32 {
                self.total.get()
            }

            /// A tally starting at the sum of the starts: a static function of the resource.
            pub fn summed(starts: Vec<u32>) -> Tally {
                Tally::new(starts.into_iter().sum())
            }
        }

        impl Drop for Tally {
            fn drop(&mut self) {
                DROPPED.with(|dropped| dropped.set(dropped.get() + 1));
            }
        }

        #[service]
        pub struct Tallies;

        impl Tallies {
            /// A tally starting at `start`.
            pub fn start(start: u32) -> Tally {
                Tally::new(start)
            }

            /// A tally for each start, in a list, and one at their sum in an option when there is
            /// more than one: owned results nested in a tuple.
            pub fn several(starts: Vec<u32>) -> (Vec<Tally>, Option<Tally>) {
                let sum = (starts.len() > 1).then(|| Tally::summed(starts.clone()));
                (starts.into_iter().map(Tally::new).collect(), sum)
            }

            /// A tally at an even start, or why not: an owned result in a `Result`.
            pub fn even(start: u32) -> Result<Tally, String> {
                if start.is_multiple_of(2) {
                    Ok(Tally::new(start))
                } else {
                    Err(format!("{start} is odd"))
                }
            }

            /// How many tallies were dropped.
            pub fn dropped() -> u32 {
                DROPPED.with(Cell::get)
            }
        }
    }

    /// Takes and returns what the boundary checks: arguments that do not read as the parameters
    /// fail a call before its method runs, and a result the boundary cannot carry fails it after.
    pub mod checks {
        /// A point of a grid.
        pub struct Point {
            pub x: i32,
            pub y: i32,
        }

        /// A colour.
        #[derive(Debug)]
        pub enum Colour {
            Red,
            Green,
        }

        /// A shape, with a size or none.
        pub enum Shape {
            Dot,
            Square(u32),
        }

        /// Switches.
        pub struct Switches {
            pub on: bool,
            pub loud: bool,
        }

        #[service]
        pub struct Checks;

        impl Checks {
            /// The sum of two bytes.
            pub fn add(a: u8, b: u8) -> u16 {
                u16::from(a) + u16::from(b)
            }

            /// The quotient, which the boundary carries only when finite.
            pub fn ratio(dividend: f32, divisor: f32) -> f32 {
                dividend / divisor
            }

            /// The code point of a character.
            pub fn code(character: char) -> u32 {
                u32::from(character)
            }

            /// The point moved by `by` along each axis.
            pub fn shift(point: Point, by: i32) -> Point {
                Point {
                    x: point.x + by,
                    y: point.y + by,
                }
            }

            /// The name of the colour.
            pub fn named(colour: Colour) -> String {
                format!("{colour:?}")
            }

            /// The area of the shape.
            pub fn area(shape: Shape) -> u64 {
                match shape {
                    Shape::Dot => 0,
                    Shape::Square(side) => u64::from(side) * u64::from(side),
                }
            }

            /// The switches, each turned the other way.
            pub fn flipped(switches: Switches) -> Switches {
                Switches {
                    on: !switches.on,
                    loud: !switches.loud,
                }
            }

            /// The value, or 0 without one.
            pub fn or_zero(value: Option<u64>) -> u64 {
                value.unwrap_or(0)
            }

            /// Half of an even value, or why not.
            pub fn halve(value: u32) -> Result<u32, String> {
                if value.is_multiple_of(2) {
                    Ok(value / 2)
                } else {
                    Err(format!("{value} is odd"))
                }
            }
        }
    }

    /// Names the types of `checks` as rustc resolves them: through a `use` that renames the type
    /// and through an alias that is only a path, which the WIT spells `point` and `colour`.
    pub mod names {
        use super::checks::Point as Spot;

        /// The colour of `checks`, under another name.
        pub type Hue = super::checks::Colour;

        #[service]
        pub struct Names;

        impl Names {
            /// The point mirrored through the origin.
            pub fn mirrored(spot: Spot) -> Spot {
                Spot {
                    x: -spot.x,
                    y: -spot.y,
                }
            }

            /// Whether the colour is red.
            pub fn red(hue: Hue) -> bool {
                matches!(hue, Hue::Red)
            }
        }
    }

    /// Two resources of the Orb's root, the cache depending on the database, whose rows its
    /// `#[open]` function reads: the cache comes first here, and opens after the database.
    pub mod cellar {
        use core::cell::Cell;

        /// The rows the database held when the cache opened.
        #[resource(depends = [Db])]
        pub struct Cache {
            pub seen: u32,
        }

        impl Cache {
            #[open]
            pub fn open() -> Cache {
                Cache {
                    seen: Db::with(|db| db.rows.get()),
                }
            }
        }

        /// The database, and its rows.
        #[resource]
        pub struct Db {
            pub rows: Cell<u32>,
        }

        impl Db {
            #[open]
            pub fn open() -> Db {
                Db { rows: Cell::new(3) }
            }
        }

        pub mod stock {
            use super::{Cache, Db};

            #[service]
            pub struct Stock;

            impl Stock {
                /// Add rows to the database: the rows the cache saw when it opened, and the
                /// database's rows now.
                pub fn add(rows: u32) -> (u32, u32) {
                    let now = Db::with(|db| {
                        db.rows.set(db.rows.get() + rows);
                        db.rows.get()
                    });
                    (Cache::with(|cache| cache.seen), now)
                }
            }
        }
    }
}
