use alloc::string::String;

/// Note what a resource went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the note goes to the journal the native tests read.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    quay::context::log(quay::context::Level::Info, &event);
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

/// A resource of each object of the service `Barge`, its fender, in an Orb whose root owns no
/// resource: the world exports `service-resources-init` alone, and natively the Orb's create
/// declares nothing while each barge's factory declares its fender.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:quay@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:quay@1.0.0", context)
)]
pub mod quay {
    pub mod barge {
        use alloc::format;

        /// Bumps the quay, cushioned by its fender.
        #[service]
        pub struct Barge;

        impl Barge {
            /// Bump once more, and answer how many times this barge bumped.
            pub fn bump() -> u32 {
                Fender::with(|fender| {
                    fender.bumps += 1;
                    fender.bumps
                })
            }
        }

        /// The fender of one barge, counting its bumps.
        #[resource]
        pub struct Fender {
            pub bumps: u32,
        }

        impl Fender {
            #[open]
            pub fn open() -> Fender {
                super::super::note("open fender".into());
                Fender { bumps: 0 }
            }

            #[close]
            pub fn close(self) {
                super::super::note(format!("close fender, {} bumps", self.bumps));
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{JOURNAL, quay};
    use crate::native::{call, hosting};
    use serde_json::json;
    use std::rc::Rc;

    /// What the resources went through since the last call.
    fn journal() -> Vec<String> {
        JOURNAL.with(|journal| journal.take())
    }

    /// Natively, without a root resource, the host opens a barge's fender when it creates the
    /// barge and closes it when the barge goes; each barge counts its own bumps.
    #[test]
    fn a_service_resource_opens_without_a_root_resource_natively() {
        let (mut pool, host) = hosting(quay::orb().unwrap());
        let mut barge = |key: &str| {
            let barge = Rc::new(
                host.get(
                    "tembra-orb-runtime-fixture::example:quay/barge@1.0.0",
                    vec![key.into()],
                    "alice",
                )
                .unwrap(),
            );
            pool.run_until(barge.ready()).unwrap();
            barge
        };
        let one = barge("one");
        let two = barge("two");
        assert_eq!(journal(), ["open fender", "open fender"]);
        assert_eq!(call(&mut pool, &host, &one, "alice", "bump", json!([])), 1);
        assert_eq!(call(&mut pool, &host, &one, "alice", "bump", json!([])), 2);
        assert_eq!(call(&mut pool, &host, &two, "alice", "bump", json!([])), 1);
        one.release().unwrap();
        pool.run_until(host.settled());
        assert_eq!(journal(), ["close fender, 2 bumps"]);
        two.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(journal(), ["close fender, 1 bumps"]);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
