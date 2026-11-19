use alloc::string::String;

/// Note what a resource went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the note goes to the journal, which no test reads, since
/// the native side does not open a service's resources yet.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    dock::context::log(dock::context::Level::Info, &event);
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

/// A resource of the Orb's root, the harbor, and one of each object of the service `Ship`, its
/// mooring, `#[resource(depends = [Harbor])]` in the service's module: the kernel opens the harbor
/// at activation, and a ship's mooring when it creates the ship, whose `#[open]` function takes
/// the next berth of the harbor, and closes the mooring when the ship goes. The harbor has two
/// berths, so the third ship's mooring answers `Err`, which fails that ship's creation.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:dock@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:dock@1.0.0", context)
)]
pub mod dock {
    use alloc::format;

    /// The berths, and how many ships took one.
    #[resource]
    pub struct Harbor {
        pub berths: u32,
        pub taken: u32,
    }

    impl Harbor {
        #[open]
        pub fn open() -> Harbor {
            super::note("open harbor".into());
            Harbor { berths: 2, taken: 0 }
        }

        #[close]
        pub fn close(self) {
            super::note(format!("close harbor, {} taken", self.taken));
        }
    }

    pub mod ship {
        use super::Harbor;
        use alloc::{format, string::String};

        /// Moors at a berth of the harbor.
        #[service]
        pub struct Ship;

        impl Ship {
            /// The berth this ship moors at.
            pub fn berth() -> u32 {
                Mooring::with(|mooring| mooring.berth)
            }
        }

        /// The berth of one ship, taken from the harbor when the kernel creates the ship.
        #[resource(depends = [Harbor])]
        pub struct Mooring {
            pub berth: u32,
        }

        impl Mooring {
            #[open]
            pub fn open() -> Result<Mooring, String> {
                let berth = Harbor::with(|harbor| {
                    if harbor.taken < harbor.berths {
                        harbor.taken += 1;
                        Some(harbor.taken)
                    } else {
                        None
                    }
                });
                match berth {
                    Some(berth) => {
                        super::super::note(format!("open mooring {berth}"));
                        Ok(Mooring { berth })
                    }
                    None => Err(String::from("no berth is free")),
                }
            }

            #[close]
            pub fn close(self) {
                super::super::note(format!("close mooring {}", self.berth));
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::dock;

    /// The native side does not open a service's resources yet: `orb()` answers an error naming
    /// the mooring, and only a host that installs the Orb fails.
    #[test]
    fn a_service_resource_is_refused_natively() {
        assert_eq!(
            crate::native::refused(dock::orb()),
            "the resource `mooring` of each object of the service `Ship` is not supported natively yet: the native side declares the resources of the Orb's root alone, so a service resource works in a component build only"
        );
    }
}
