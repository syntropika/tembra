//! A compiled Orb handing out text builders. `Builder` is a WIT resource: a value that stays in
//! this component while whoever started it holds a handle and calls its methods, one character per
//! call, until it drops the handle and the kernel destroys the value. The macro derives the
//! `resource` from the struct, its `#[constructor]` and its `&self` methods.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(no_std, package = "example:text@1.0.0")]
mod text {
    use alloc::string::String;
    use core::cell::{Cell, RefCell};

    /// How many builders this instance has destroyed, for the `dropped` method.
    struct Dropped(Cell<u32>);
    // SAFETY: a component runs one export at a time on one thread.
    unsafe impl Sync for Dropped {}
    static DROPPED: Dropped = Dropped(Cell::new(0));

    /// A text under construction: the caller owns it and pushes one character per call.
    #[wit_resource]
    pub struct Builder {
        text: RefCell<String>,
    }

    impl Builder {
        /// An empty text: the constructor of the resource, the function marked `#[constructor]`.
        #[constructor]
        pub fn new() -> Builder {
            Builder {
                text: RefCell::new(String::new()),
            }
        }

        /// Append one character and return how many the text holds; a method of the resource.
        pub fn push(&self, ch: char) -> u32 {
            let mut text = self.text.borrow_mut();
            text.push(ch);
            text.chars().count() as u32
        }

        /// The text built so far.
        pub fn text(&self) -> String {
            self.text.borrow().clone()
        }
    }

    impl Drop for Builder {
        fn drop(&mut self) {
            DROPPED.0.set(DROPPED.0.get() + 1);
        }
    }

    /// The service: starts builders and counts the ones destroyed.
    #[service]
    pub struct Texts;

    impl Texts {
        /// Start an empty text; the caller owns the returned handle.
        pub fn start() -> Builder {
            Builder::new()
        }

        /// How many builders were destroyed, when their last owner dropped them.
        pub fn dropped() -> u32 {
            DROPPED.0.get()
        }
    }
}
