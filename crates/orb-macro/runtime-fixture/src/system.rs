/// Imports WASI interfaces of the host's system capabilities, besides the random source and the
/// monotonic clock: the system clock, the clocks' types, which ask nothing of a host, the CLI's
/// environment and the insecure random source. An installation grants `clocks`, `cli` and `random`.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:system@1.0.0",
        import = "wasi:clocks/system-clock@0.3.0",
        import = "wasi:clocks/types@0.3.0",
        import = "wasi:cli/environment@0.3.0",
        import = "wasi:random/insecure@0.3.0",
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:system@1.0.0",
        import = "wasi:clocks/system-clock@0.3.0",
        import = "wasi:clocks/types@0.3.0",
        import = "wasi:cli/environment@0.3.0",
        import = "wasi:random/insecure@0.3.0"
    )
)]
pub mod system {
    use alloc::{format, string::String, vec::Vec};

    /// Reads the host through WASI.
    #[service]
    pub struct Probe;

    impl Probe {
        /// The seconds of the host's system clock since the epoch.
        pub fn seconds() -> i64 {
            system_clock::now().seconds
        }

        /// The nanoseconds of `seconds` seconds, a duration of the clocks' types.
        pub fn nanoseconds(seconds: u64) -> u64 {
            let duration: types::Duration = seconds * 1_000_000_000;
            duration
        }

        /// The arguments and the environment the installation gives, as `name=value`.
        pub fn environment() -> Vec<String> {
            let variables = environment::get_environment()
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"));
            environment::get_arguments()
                .into_iter()
                .chain(variables)
                .collect()
        }

        /// How many insecure random bytes the host gives for `length`.
        pub fn insecure(length: u64) -> u32 {
            insecure::get_insecure_random_bytes(length).len() as u32
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    /// Natively, the module compiles and `orb()` answers why a host cannot install it: the native
    /// side does not serve a WASI import yet.
    #[test]
    fn a_wasi_import_is_refused_natively() {
        assert_eq!(
            crate::native::refused(super::system::orb()),
            "the import of wasi:clocks/system-clock@0.3.0 is not supported natively yet"
        );
    }
}
