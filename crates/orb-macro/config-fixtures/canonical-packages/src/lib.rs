//! Compose canonical packages without a copied dependency tree.
#[tembra::orb(
    package = "example:composed@1.0.0",
    wit = ["model", "metadata", "tools", "context"],
    contract = env!("OUT_DIR"),
    import = "example:model-agent/agent@1.0.0"
)]
pub mod composed {
    /// A record derived from a canonical imported type.
    pub struct Session {
        pub model: agent::Model,
    }
    /// Inspect the imported model.
    #[service]
    pub struct Api;
    impl Api {
        /// Preserve the canonical package identity inside a derived record.
        pub async fn describe() -> Session {
            Session {
                model: agent::describe().await,
            }
        }
    }
}
