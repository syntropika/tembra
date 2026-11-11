//! A compiled Orb handing out sessions. `Session` is a WIT resource: a value that stays in this
//! component while whoever opened it holds a handle and calls its methods, as many times as it
//! likes, until it drops the handle and the kernel destroys the value. The macro derives the
//! `resource` from the struct and its `&self` methods.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:decoder@1.0.0")]
mod decoder {
    use alloc::vec::Vec;
    use core::cell::Cell;

    /// How many sessions this instance has destroyed, for the `closed` method.
    struct Closed(Cell<u32>);
    // SAFETY: a component runs one export at a time on one thread.
    unsafe impl Sync for Closed {}
    static CLOSED: Closed = Closed(Cell::new(0));

    /// One decoding session: the caller owns it and decodes frame after frame with it.
    #[wit_resource]
    pub struct Session {
        key: u8,
        frames: Cell<u32>,
    }

    impl Session {
        /// A session keyed by `key`; the constructor of the WIT resource.
        pub fn new(key: u8) -> Session {
            Session {
                key,
                frames: Cell::new(0),
            }
        }

        /// Decode one frame: here, undo an exclusive-or with the key; a method of the resource.
        pub fn decode(&self, frame: Vec<u8>) -> Vec<u8> {
            self.frames.set(self.frames.get() + 1);
            frame.into_iter().map(|byte| byte ^ self.key).collect()
        }

        /// How many frames this session decoded so far.
        pub fn frames(&self) -> u32 {
            self.frames.get()
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            CLOSED.0.set(CLOSED.0.get() + 1);
        }
    }

    /// The service: opens sessions and counts the ones destroyed.
    #[service]
    pub struct Decoding;

    impl Decoding {
        /// Open a session for `key`; the caller owns the returned handle.
        pub fn open(key: u8) -> Session {
            Session::new(key)
        }

        /// How many sessions were destroyed, when their last owner dropped them.
        pub fn closed() -> u32 {
            CLOSED.0.get()
        }
    }
}
