use alloc::string::String;

/// Note what a lamp went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the `context` serves only a call's methods, so the
/// note goes to the journal the native tests read.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    lamps::context::log(lamps::context::Level::Info, &event);
    #[cfg(not(target_arch = "wasm32"))]
    JOURNAL.with(|journal| journal.borrow_mut().push(event));
}

#[cfg(not(target_arch = "wasm32"))]
std::thread_local! {
    /// What the lamps of the Orbs this thread hosts went through, natively, in order.
    static JOURNAL: core::cell::RefCell<alloc::vec::Vec<String>> = const {
        core::cell::RefCell::new(alloc::vec::Vec::new())
    };
}

/// A lamp, a resource of the Orb's root at version 1, whose `#[open]` and `#[close]` functions note
/// what they do: the first revision of the Orb. With the `revised` feature the module is the next
/// revision, below, whose lamp is at version 2.
#[cfg(not(feature = "revised"))]
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:lamps@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:lamps@1.0.0", context)
)]
pub mod lamps {
    use alloc::format;
    use core::cell::Cell;

    /// The lamp, and how many times it was lit.
    #[resource(version = 1)]
    pub struct Lamp {
        pub lit: Cell<u32>,
    }

    impl Lamp {
        #[open]
        pub fn open() -> Lamp {
            super::note("open 1".into());
            Lamp { lit: Cell::new(0) }
        }

        #[close]
        pub fn close(self) {
            super::note(format!("close 1, lit {}", self.lit.get()));
        }
    }

    /// Lights the lamp.
    #[service]
    pub struct Switch;

    impl Switch {
        /// Light the lamp and return how many times it was lit.
        pub fn light() -> u32 {
            Lamp::with(|lamp| {
                lamp.lit.set(lamp.lit.get() + 1);
                lamp.lit.get()
            })
        }
    }
}

/// The lamps' next revision, whose lamp is at version 2.
#[cfg(feature = "revised")]
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:lamps@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:lamps@1.0.0", context)
)]
pub mod lamps {
    use alloc::format;
    use core::cell::Cell;

    /// The lamp, and how many times it was lit.
    #[resource(version = 2)]
    pub struct Lamp {
        pub lit: Cell<u32>,
    }

    impl Lamp {
        #[open]
        pub fn open() -> Lamp {
            super::note("open 2".into());
            Lamp { lit: Cell::new(0) }
        }

        #[close]
        pub fn close(self) {
            super::note(format!("close 2, lit {}", self.lit.get()));
        }
    }

    /// Lights the lamp.
    #[service]
    pub struct Switch;

    impl Switch {
        /// Light the lamp and return how many times it was lit.
        pub fn light() -> u32 {
            Lamp::with(|lamp| {
                lamp.lit.set(lamp.lit.get() + 1);
                lamp.lit.get()
            })
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{JOURNAL, lamps};
    use crate::native::{call, hosting, ready};
    use serde_json::json;

    /// What the lamps went through since the last call.
    fn journal() -> Vec<String> {
        JOURNAL.with(|journal| journal.take())
    }

    /// Natively, the host opens the lamp of the revision built, at its version, once the Orb is
    /// installed; a reload of the same code keeps the lamp, lit as it was, since its version stays
    /// the same; and the host closes it, through its `#[close]` function, when it closes.
    #[test]
    fn the_lamp_opens_and_closes_natively() {
        let version = if cfg!(feature = "revised") { 2 } else { 1 };
        let (mut pool, host) = hosting(lamps::orb().unwrap());
        let switch = ready(&mut pool, &host, "example:lamps/switch@1.0.0");
        let light = |pool: &mut _| call(pool, &host, &switch, "alice", "light", json!([]));
        assert_eq!(light(&mut pool), json!(1));
        assert_eq!(light(&mut pool), json!(2));
        assert_eq!(journal(), [format!("open {version}")]);
        pool.run_until(host.reload(vec![lamps::orb().unwrap()], 5_000))
            .unwrap();
        assert_eq!(light(&mut pool), json!(3));
        assert!(journal().is_empty());
        switch.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(journal(), [format!("close {version}, lit 3")]);
    }
}
