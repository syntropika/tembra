//! Fixed-WIT codec qualification shared with the component fixture.
#[tembra::orb(
    package = "example:codec-provider@1.0.0",
    wit = "../native-fixture/wit"
)]
pub mod provider {
    pub mod implementation {
        
        use std::cell::Cell;
        thread_local! { static DROPPED: Cell<u32> = const { Cell::new(0) }; }
    
        /// The existing provider protocol, whose pure functions remain synchronous.
        #[service("example:protocol/codec@1.0.0")]
        pub struct Codec;
    
        impl Codec {
            /// Normalize a request without changing the synchronous WIT contract.
            pub fn normalize(value: u64) -> u64 {
                value + 1
            }
            /// Retain owned strings and lists inside synchronous WIT containers.
            pub fn echo(
                value: (Option<String>, Result<Vec<String>, String>),
            ) -> (Option<String>, Result<Vec<String>, String>) {
                value
            }
            /// Return a decoder owned by this provider.
            pub fn open(seed: u64) -> Decoder {
                Decoder::new(seed)
            }
            /// Return independently owned resources inside supported containers.
            pub fn pair(seed: u64) -> (Decoder, Vec<Option<Decoder>>) {
                (Decoder::new(seed), vec![None, Some(Decoder::new(seed + 1))])
            }
            /// Count physical decoder destruction.
            pub fn dropped() -> u32 {
                DROPPED.with(Cell::get)
            }
        }
    
        /// One provider-owned decoder.
        #[wit_resource]
        pub struct Decoder {
            total: Cell<u64>,
        }
    
        impl Decoder {
            /// Construct a decoder with its own identity and state.
            #[constructor]
            pub fn new(seed: u64) -> Self {
                Self {
                    total: Cell::new(seed),
                }
            }
            /// A synchronous static factory.
            pub fn zero() -> Decoder {
                Self::new(0)
            }
            /// Read the decoder's state synchronously.
            pub fn total(&self) -> u64 {
                self.total.get()
            }
            /// Borrow the decoder while accepting an owned string in the common import API.
            pub fn label(&self, value: String) -> String {
                format!("{}:{value}", self.total.get())
            }
            /// Suspend for one scheduler turn and resume with the same decoder.
            pub async fn next(&self) -> Option<u64> {
                let mut pending = true;
                core::future::poll_fn(|cx| {
                    if pending {
                        pending = false;
                        cx.waker().wake_by_ref();
                        core::task::Poll::Pending
                    } else {
                        core::task::Poll::Ready(())
                    }
                })
                .await;
                self.total.set(self.total.get() + 1);
                Some(self.total.get())
            }
        }
    
        impl Drop for Decoder {
            fn drop(&mut self) {
                DROPPED.with(|n| n.set(n.get() + 1));
            }
        }
    }
}
