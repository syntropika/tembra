//! One Orb that builds natively and as a component, so that a test compares the two:
//! `executor-wasm/tests/orb_macro_parity.rs` installs `parity::orb(..)` in one host and the
//! component `just test-orb-parity` builds from this crate in another, makes the same calls on both
//! and requires the same answers, as the macro promises that a module behaves alike either way.
//!
//! Each module of the Orb serves one feature of that test's list. `imports` turns the dials of
//! another Orb, through the plain import and the view of the import by key; `cancellation` stops
//! awaiting a call of the plain import; `ledger` keeps a state; `tallies` issues WIT resources;
//! and `checks` takes and returns what the boundary checks. A new scenario is a module here, or a
//! function of one, and a row of the test's list. The Orb holds nothing a native host refuses by
//! design, a hook declaration, a synchronous import or a WASI import, and nothing native only.

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
        pub struct Entry {
            pub label: String,
            pub amount: u64,
        }

        /// What moved the balance last.
        pub enum Movement {
            Opened,
            Deposit(u64),
            Transfer { to: String, amount: u64 },
        }

        /// What the book went through. The state's `Default` starts it from its own.
        #[derive(Default)]
        pub struct Marks {
            pub audited: bool,
            pub disputed: bool,
        }

        /// The book of one ledger object, which the kernel keeps.
        #[state(service = Ledger)]
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
}
