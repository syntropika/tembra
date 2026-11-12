//! A compiled Orb contributing to another Orb's hook. What it counts lives in a resource of its
//! root: the kernel opens it when the Orb is activated and closes it when the Orb is torn down,
//! and the value stays in the component, so a reload starts it again (see examples/rust-orb-state
//! for a service state the kernel keeps across reloads). A contribution runs in the Orb's root,
//! which owns no kernel state; a resource is what it can use.
#![cfg_attr(target_arch = "wasm32", no_std)]

#[tembra::orb(no_std, package = "example:filter@1.0.0", wit = "../wit")]
mod filter {
    /// What this contributor observed: admissions seen, and how many it blocked.
    #[resource]
    pub struct Counts {
        pub seen: u32,
        pub blocked: u32,
    }

    impl Counts {
        /// Opened by the kernel when the Orb is activated: the function marked `#[open]`.
        #[open]
        pub fn open() -> Counts {
            Counts {
                seen: 0,
                blocked: 0,
            }
        }
    }

    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:admission/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        let blocked = input.value < 0;
        Counts::with(|counts| {
            counts.seen += 1;
            if blocked {
                counts.blocked += 1;
            }
        });
        if blocked {
            return guard::Admission {
                value: input.value,
                blocked: true,
            };
        }
        input
    }

    /// The contributor's own service.
    #[service]
    pub struct Report;

    impl Report {
        /// Admissions seen and blocked so far by this instance of the Orb.
        pub fn read() -> Counts {
            Counts::with(|counts| *counts)
        }
    }
}
