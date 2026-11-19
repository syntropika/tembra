//! Fixed-WIT provider and consumer of owned decoder resources.
pub mod consumer;
pub mod provider;
pub use consumer::consumer as importer;
pub use provider::provider as exporter;

mod packets;
pub use packets::packet_reader;

#[tembra::orb(
    package = "example:sink-reader@1.0.0",
    wit = "wit",
    import = "example:protocol/sink@1.0.0"
)]
pub mod sink_reader {
    pub mod probe {
        
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
}
