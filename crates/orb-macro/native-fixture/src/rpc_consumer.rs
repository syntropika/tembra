//! One consumer source using the complete awaitable API on native and component builds.
#[tembra::orb(
    package = "example:rpc-consumer@1.0.0",
    wit = "../native-fixture/wit",
    import = "example:rpc-resources/codec@1.0.0"
)]
pub mod consumer {
    /// Compose synchronous factories, owned arguments and asynchronous decoder methods.
    #[service]
    pub struct Pipeline;

    impl Pipeline {
        /// Move the original owners through methods, aliases and supported containers.
        pub async fn compose(seed: u64) -> u64 {
            let decoder: codec::DecoderAlias = codec::Decoder::new(seed).await;
            decoder.merge(codec::Decoder::zero().await).await;
            let (first, rest) = codec::pair(seed + 1).await;
            codec::consume_many((
                vec![Some(decoder), None, Some(first)],
                Ok(rest.into_iter().flatten().next().unwrap()),
            ))
            .await
                + codec::consume(codec::open(seed + 3).await).await
        }

        /// Suspend on a genuine asynchronous import while borrowing the same decoder.
        pub async fn advance(seed: u64) -> u64 {
            let decoder = codec::open(seed).await;
            decoder.next().await.unwrap()
        }
    }
}
