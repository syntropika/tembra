//! A compiled Orb that reads the kernel's view of its calls: who is calling, which object answers,
//! whether the call was cancelled, and a log the host observes. `context` on the attribute imports
//! the kernel's `tembra:guest/context` interface; the executor serves it, the installer grants nothing.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:audit@1.0.0", context)]
mod audit {
    use alloc::string::String;

    /// Who called, and which object answered.
    pub struct Caller {
        /// The principal of the call: the root it runs under.
        pub principal: String,
        /// The key of the object running the call, as its reference named it.
        pub key: Vec<String>,
        /// The principal that acquired the object's reference.
        pub owner: String,
    }

    /// The service: it reports its callers and writes to the host's log.
    #[service]
    pub struct Auditor;

    impl Auditor {
        /// The call's principal and the address of the object running it; a log record too.
        pub fn whoami() -> Caller {
            let object = context::object();
            context::log(context::Level::Info, "whoami was asked");
            Caller {
                principal: context::principal(),
                key: object.key,
                owner: object.principal,
            }
        }

        /// Write a message to the host's log, at level warn, attributed to this Orb by the host.
        pub fn warn(message: String) {
            context::log(context::Level::Warn, &message);
        }

        /// Cancel the current call from inside, then report whether the kernel sees it cancelled.
        pub fn give_up() -> bool {
            context::cancel();
            context::cancelled()
        }
    }
}
