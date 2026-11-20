//! Fixed-WIT resource containers shared with the compiled import fixture.
#[tembra::orb(
    package = "example:packet-reader@1.0.0",
    wit = "../native-fixture/wit",
    import = "example:protocol/packets@1.0.0"
)]
pub mod packet_reader {
    pub mod reader {
        use super::packets;
        /// Read resources nested through every supported portable container.
        #[service]
        pub struct Reader;
        impl Reader {
            /// Abandon a resource method after admission; the importing callback joins its work.
            pub async fn glance() -> u64 {
                let received = packets::receive().await;
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
                let received = packets::receive().await;
                let (batch, ticket) = received.unwrap();
                let mut total = ticket.value().await;
                for event in batch.into_iter().flatten() {
                    if let packets::Event::Created(envelope) | packets::Event::Guest(envelope) =
                        event
                    {
                        total += envelope.amount + envelope.ticket.value().await;
                    }
                }
                total
            }
        }
    }
}
