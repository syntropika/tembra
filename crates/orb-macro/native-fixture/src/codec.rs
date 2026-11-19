//! Fixed-WIT provider and consumer of owned decoder resources.
pub mod consumer;
pub mod provider;
pub use consumer::consumer as importer;
pub use provider::provider as exporter;

#[tembra::orb(
    package = "example:packet-reader@1.0.0",
    wit = "wit",
    import = "example:protocol/packets@1.0.0"
)]
pub mod packet_reader {
    /// Read resources nested through every supported portable container.
    #[service]
    pub struct Reader;
    impl Reader {
        /// Abandon a resource method after admission; the importing callback joins its work.
        pub async fn glance() -> u64 {
            #[cfg(not(target_arch = "wasm32"))]
            let received = packets::receive().await;
            #[cfg(target_arch = "wasm32")]
            let received = packets::receive();
            let (batch, ticket) = received.unwrap();
            drop(batch);
            {
                let mut called = core::pin::pin!(ticket.value());
                core::future::poll_fn(|cx| {
                    let _ = core::future::Future::poll(called.as_mut(), cx);
                    core::task::Poll::Ready(())
                })
                .await;
            }
            0
        }

        /// Read exact resource identities inside a result, tuple, list, option, variant and record.
        pub async fn read() -> u64 {
            #[cfg(not(target_arch = "wasm32"))]
            let received = packets::receive().await;
            #[cfg(target_arch = "wasm32")]
            let received = packets::receive();
            let (batch, ticket) = received.unwrap();
            let mut total = ticket.value().await;
            for event in batch.into_iter().flatten() {
                if let packets::Event::Created(envelope) = event {
                    total += envelope.amount + envelope.ticket.value().await;
                }
            }
            total
        }
    }
}

#[tembra::orb(
    package = "example:sink-reader@1.0.0",
    wit = "wit",
    import = "example:protocol/sink@1.0.0"
)]
pub mod sink_reader {
    /// Import a foreign owned argument without relabeling its nominal resource type.
    #[service]
    pub struct Probe;
    impl Probe {
        /// This module's imported alias compiles even without a callable defining import.
        pub fn available() -> bool {
            true
        }
    }
}
