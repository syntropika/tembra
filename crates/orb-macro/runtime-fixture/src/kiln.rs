use alloc::string::String;

/// Note what a resource went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the note goes to the journal the native tests read.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    kiln::context::log(kiln::context::Level::Info, &event);
    #[cfg(not(target_arch = "wasm32"))]
    JOURNAL.with(|journal| journal.borrow_mut().push(event));
}

#[cfg(not(target_arch = "wasm32"))]
std::thread_local! {
    /// What the resources of the Orbs this thread hosts went through, natively, in order.
    static JOURNAL: core::cell::RefCell<alloc::vec::Vec<String>> = const {
        core::cell::RefCell::new(alloc::vec::Vec::new())
    };
}

/// Two resources of the Orb's root whose `#[open]` functions return a `Result`: the wood opens,
/// and the fire, which depends on it, answers an error, which fails the Orb's activation, after
/// which the kernel closes the wood. The wood names its struct by a path, and the fire returns an
/// alias of `Result` with one type argument.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:kiln@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:kiln@1.0.0", context)
)]
pub mod kiln {
    use alloc::string::String;

    /// The wood, which opens.
    #[resource]
    pub struct Wood {
        pub logs: u32,
    }

    impl Wood {
        #[open]
        pub fn open() -> Result<self::Wood, String> {
            super::note("open wood".into());
            Ok(Wood { logs: 2 })
        }

        #[close]
        pub fn close(self) {
            super::note("close wood".into());
        }
    }

    /// The fire, which cannot catch.
    #[resource(depends = [Wood])]
    pub struct Fire {
        pub heat: u32,
    }

    impl Fire {
        #[open]
        pub fn open() -> fail::Result<Fire> {
            Err(Damp(Wood::with(|wood| wood.logs)))
        }
    }

    /// Why the fire does not catch: the logs are damp.
    pub struct Damp(pub u32);

    /// The results of the kiln's opens.
    pub mod fail {
        /// A result whose error is damp logs.
        pub type Result<T> = core::result::Result<T, super::Damp>;
    }

    impl core::fmt::Display for Damp {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(formatter, "{} damp logs", self.0)
        }
    }

    pub mod hearth {
        use super::Fire;

        /// Reads the fire.
        #[service]
        pub struct Hearth;

        impl Hearth {
            /// The fire's heat.
            pub fn heat() -> u32 {
                Fire::with(|fire| fire.heat)
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{JOURNAL, kiln};
    use crate::native::{ORB, hosting};
    use tembra::core::Code;

    /// Natively, the fire's `#[open]` answers an error: the activation fails with it, worded as
    /// a component's, and the host closes the wood, which opened before it.
    #[test]
    fn an_open_that_answers_an_error_fails_the_activation_natively() {
        let (mut pool, host) = hosting(kiln::orb().unwrap());
        let hearth = host
            .get(
                &format!("{ORB}::example:kiln/hearth@1.0.0"),
                vec!["main".into()],
                "alice",
            )
            .unwrap();
        let refused = pool
            .run_until(hearth.ready())
            .expect_err("the activation fails");
        assert_eq!(
            (refused.code, refused.reason.as_str(), refused.message.as_str()),
            (
                Code::Unavailable,
                "resource",
                "the resource `fire` did not open: 2 damp logs"
            ),
            "{refused}"
        );
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        assert_eq!(
            JOURNAL.with(|journal| journal.take()),
            ["open wood", "close wood"]
        );
    }
}
