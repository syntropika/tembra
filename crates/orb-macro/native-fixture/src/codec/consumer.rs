//! Fixed-WIT codec qualification shared with the component fixture.
#[tembra::orb(
    package = "example:codec-consumer@1.0.0",
    wit = "../native-fixture/wit",
    import = "example:protocol/codec@1.0.0"
)]
pub mod consumer {
    use std::{cell::RefCell, rc::Rc};
    thread_local! {
        static HELD: RefCell<Option<Rc<codec::Decoder>>> = const { RefCell::new(None) };
    }

    /// Compose the provider through its WIT import.
    #[service]
    pub struct Pipeline;

    impl Pipeline {
        /// Retain a provider-owned decoder across importing callbacks.
        pub async fn keep(seed: u64) {
            #[cfg(not(target_arch = "wasm32"))]
            let decoder = codec::open(seed).await;
            #[cfg(target_arch = "wasm32")]
            let decoder = codec::open(seed);
            HELD.with(|held| held.replace(Some(Rc::new(decoder))));
        }

        /// Call the retained decoder under the current callback's authority.
        pub async fn read() -> u64 {
            let decoder = HELD
                .with(|held| held.borrow().clone())
                .expect("a retained decoder");
            #[cfg(not(target_arch = "wasm32"))]
            let total = decoder.total().await;
            #[cfg(target_arch = "wasm32")]
            let total = decoder.total();
            total
        }

        /// Release a retained owner through the callback's normal settlement path.
        pub fn discard() {
            HELD.with(|held| held.take());
        }

        /// Exercise sync functions, constructors, statics, methods, containers and WIT drops.
        pub async fn run(seed: u64) -> (u64, u32) {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let start = codec::dropped().await;
                let seed = codec::normalize(seed).await;
                let first = codec::open(seed).await;
                let constructed = codec::Decoder::new(seed + 1).await;
                let zero = codec::Decoder::zero().await;
                let (paired, nested) = codec::pair(seed + 2).await;
                let next = first.next().await.unwrap();
                let total = first.total().await
                    + constructed.total().await
                    + zero.total().await
                    + paired.total().await
                    + nested[1].as_ref().unwrap().total().await;
                assert_eq!(next, first.total().await);
                drop((first, constructed, zero, paired, nested));
                (total, codec::dropped().await - start)
            }
            #[cfg(target_arch = "wasm32")]
            {
                let start = codec::dropped();
                let seed = codec::normalize(seed);
                let first = codec::open(seed);
                let constructed = codec::Decoder::new(seed + 1);
                let zero = codec::Decoder::zero();
                let (paired, nested) = codec::pair(seed + 2);
                let next = first.next().await.unwrap();
                let total = first.total()
                    + constructed.total()
                    + zero.total()
                    + paired.total()
                    + nested[1].as_ref().unwrap().total();
                assert_eq!(next, first.total());
                drop((first, constructed, zero, paired, nested));
                (total, codec::dropped() - start)
            }
        }
    }
}
