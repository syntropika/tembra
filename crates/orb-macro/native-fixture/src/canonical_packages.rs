//! A provider and consumer share canonical package roots instead of copied WIT schemas.
#[tembra::orb(
    package = "example:canonical-provider@1.0.0",
    wit = [
        "../config-fixtures/canonical-packages/wit/model",
        "../config-fixtures/canonical-packages/wit/metadata",
        "../config-fixtures/canonical-packages/wit/tools",
        "../config-fixtures/canonical-packages/wit/context"
    ],
    import = "example:context/context@1.0.0"
)]
pub mod provider {
    pub mod implementation {
        use super::{agent, context};
        /// Serve the canonical model protocol.
        #[service("example:model-agent/agent@1.0.0")]
        pub struct ModelAgent;

        impl ModelAgent {
            /// Return a value spanning the entire canonical package closure.
            pub fn describe() -> agent::Model {
                agent::Model {
                    metadata: agent::Metadata {
                        context: context::Context {
                            principal: "alice".into(),
                        },
                        name: "model".into(),
                    },
                    tools: vec![agent::Tool {
                        context: context::Context {
                            principal: "alice".into(),
                        },
                        name: "browser".into(),
                    }],
                }
            }
        }
    }
}

#[tembra::orb(
    package = "example:canonical-consumer@1.0.0",
    wit = [
        "../config-fixtures/canonical-packages/wit/context",
        "../config-fixtures/canonical-packages/wit/tools",
        "../config-fixtures/canonical-packages/wit/metadata",
        "../config-fixtures/canonical-packages/wit/model"
    ],
    import = "example:model-agent/agent@1.0.0"
)]
pub mod consumer {
    pub mod implementation {
        use super::agent;
        /// A derived schema preserves the canonical record's nominal package.
        pub struct Session {
            pub model: agent::Model,
        }
        /// Compose the separate capability packages through the actual import transport.
        #[service]
        pub struct Api;
        impl Api {
            /// Return the imported model in a derived record.
            pub async fn describe() -> Session {
                Session {
                    model: agent::describe().await,
                }
            }
        }
    }
}
