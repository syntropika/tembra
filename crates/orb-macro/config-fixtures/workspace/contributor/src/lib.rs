//! A guest of the workspace that contributes to a hook, as `examples/rust-orb-hook-contribution`'s
//! filter does. Its native contribution needs the runtime's `hooks`, which the application's `host`
//! leaves out, so with it the module still compiles, and its `orb(..)` answers an error naming the
//! feature.

#[tembra::orb(package = "example:filter@1.0.0", wit = "wit")]
pub mod filter {
    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:admission/guard@1.0.0")]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        guard::Admission {
            value: input.value,
            blocked: input.blocked || input.value < 0,
        }
    }
}
