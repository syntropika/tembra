//! One fixed-WIT implementation built natively and as a real component for RPC tests.
#[tembra::orb(
    package = "example:rpc-provider@1.0.0",
    wit = "../native-fixture/wit",
    import = "example:rpc-resources/gate@1.0.0"
)]
pub mod provider {
    pub mod implementation {
        use super::gate;
        use std::cell::Cell;
        thread_local! {
            static DROPPED: Cell<u32> = const { Cell::new(0) };
            static CALLED: Cell<u32> = const { Cell::new(0) };
        }

        /// Record entry into author code independently of transport refusal.
        fn called() {
            CALLED.with(|value| value.set(value.get() + 1));
        }

        /// A decoder provider whose WIT signatures remain synchronous where declared synchronous.
        #[service("example:rpc-resources/codec@1.0.0")]
        pub struct Codec;

        impl Codec {
            /// Return an owned resource through a transparent nominal alias.
            pub fn open(seed: u64) -> Decoder {
                Decoder::new(seed)
            }

            /// Consume the original native value, rather than a second resource handle.
            pub fn consume(value: Decoder) -> u64 {
                called();
                value.total.get()
            }

            /// Consume nested own positions through the existing portable container profile.
            pub fn consume_many(values: (Vec<Option<Decoder>>, Result<Decoder, String>)) -> u64 {
                called();
                values
                    .0
                    .into_iter()
                    .flatten()
                    .map(|value| value.total.get())
                    .sum::<u64>()
                    + values.1.map(|value| value.total.get()).unwrap_or(0)
            }

            /// Return independent owners inside a tuple, list and nullable option.
            pub fn pair(seed: u64) -> (Decoder, Vec<Option<Decoder>>) {
                (Decoder::new(seed), vec![None, Some(Decoder::new(seed + 1))])
            }

            /// Preserve large integral floats without confusing them with WIT integers.
            pub fn float(value: f64) -> f64 {
                value
            }

            /// Expose test counters without entering any resource method.
            pub fn stats() -> (u32, u32) {
                (DROPPED.with(Cell::get), CALLED.with(Cell::get))
            }
        }

        /// A provider-owned resource whose state survives asynchronous method suspension.
        #[wit_resource]
        pub struct Decoder {
            total: Cell<u64>,
        }

        impl Decoder {
            /// Construct one distinct resource instance.
            #[constructor]
            pub fn new(seed: u64) -> Self {
                called();
                Self {
                    total: Cell::new(seed),
                }
            }

            /// Construct a resource through a static WIT function.
            pub fn zero() -> Decoder {
                Self::new(0)
            }

            /// Read state without changing the WIT calling convention.
            pub fn total(&self) -> u64 {
                called();
                self.total.get()
            }

            /// Apply one synchronous update with exact 64-bit values.
            pub fn push(&self, frame: u64) -> u64 {
                called();
                self.total.set(self.total.get() + frame);
                self.total.get()
            }

            /// Suspend on an existing imported service before updating this same instance.
            pub async fn next(&self) -> Option<u64> {
                self.wait().await;
                Some(self.push(1))
            }

            /// Retain the implicit receiver while the host-controlled imported call is pending.
            pub async fn wait(&self) -> u64 {
                called();
                gate::wait().await;
                self.total.get()
            }

            /// Borrow self while consuming another resource through an own argument.
            pub fn merge(&self, other: Decoder) -> u64 {
                called();
                self.total.set(self.total.get() + other.total.get());
                self.total.get()
            }
        }

        impl Drop for Decoder {
            /// Count the physical destruction of the original value exactly once.
            fn drop(&mut self) {
                DROPPED.with(|value| value.set(value.get() + 1));
            }
        }
    }
    pub mod data {
        use super::{gate, values};
        use values::{Choice, Limits};

        /// A fixed-WIT data-only interface served by both executor types.
        #[service("example:rpc-resources/values@1.0.0")]
        pub struct Values;

        impl Values {
            /// Echo integer boundaries without arithmetic or floating-point conversion.
            pub fn echo(value: Limits) -> Limits {
                value
            }

            /// Retain ordinary context cancellation while awaiting another Orb.
            pub async fn wait(value: Limits) -> Limits {
                gate::wait().await;
                value
            }

            /// Preserve every supported data container across an asynchronous WIT function.
            pub async fn nested(
                values: Vec<Option<(u64, i64)>>,
                selected: Choice,
                outcome: Result<Limits, i64>,
            ) -> (Vec<Option<(u64, i64)>>, Choice, Result<Limits, i64>) {
                (values, selected, outcome)
            }
        }
    }
}
