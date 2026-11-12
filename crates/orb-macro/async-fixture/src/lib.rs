//! A fixed-contract async Orb whose interface and resource methods suspend on a real import.
#![no_std]

#[tembra::orb(
    package = "example:consumer@1.0.0",
    wit = "../test-wit",
    import = "example:pace/clock@1.0.0",
    no_std
)]
mod events {
    #[service("example:events/feed@1.0.0")]
    pub struct Feed;

    impl Feed {
        pub async fn subscribe() -> Updates {
            Updates(core::cell::Cell::new(clock::wait(7).await))
        }

        pub fn count() -> u32 {
            1
        }
    }

    #[wit_resource(service = Feed)]
    pub struct Updates(core::cell::Cell<u64>);

    impl Updates {
        #[constructor]
        pub fn new() -> Self {
            Self(core::cell::Cell::new(0))
        }

        pub async fn next(&self) -> Option<u64> {
            let value = clock::wait(self.0.get() + 1).await;
            self.0.set(value);
            Some(value)
        }

        pub async fn fresh() -> Updates {
            Updates(core::cell::Cell::new(clock::wait(1).await))
        }
    }
}
