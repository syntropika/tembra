//! Fixed-WIT codec qualification shared with the component fixture.
#[tembra::orb(
    package = "example:codec-consumer@1.0.0",
    wit = "../native-fixture/wit",
    import = "example:protocol/codec@1.0.0"
)]
pub mod consumer {
    pub mod pipeline {
        use super::codec;

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
                let decoder = codec::open(seed).await;
                HELD.with(|held| held.replace(Some(Rc::new(decoder))));
            }
    
            /// Call the retained decoder under the current callback's authority.
            pub async fn read() -> u64 {
                let decoder = HELD
                    .with(|held| held.borrow().clone())
                    .expect("a retained decoder");
                decoder.total().await
            }
    
            /// Release a retained owner through the callback's normal settlement path.
            pub fn discard() {
                HELD.with(|held| held.take());
            }
    
            /// Exercise sync functions, constructors, statics, methods, containers and WIT drops.
            pub async fn run(seed: u64) -> (u64, u32) {
                let input = (Some(String::from("model")), Ok(vec![String::from("frame")]));
                assert_eq!(codec::echo(input.clone()).await, input);
                let start = codec::dropped().await;
                let seed = codec::normalize(seed).await;
                let first = codec::open(seed).await;
                assert_eq!(
                    first.label(String::from("decoder")).await,
                    format!("{seed}:decoder")
                );
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
        }
    }
}
