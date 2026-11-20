//! A fixed-WIT contribution shared by native and compiled hook matrix tests.
#[tembra::orb(
    package = "example:hook-watch@1.0.0",
    wit = "../runtime-fixture/admission"
)]
pub mod hook_watch {
    /// Raise before blocking, through the canonical callback bridge.
    #[hook("example:admission/guard@1.0.0", priority = 0)]
    pub fn raise(input: guard::Admission) -> guard::Verdict {
        let value = input.value + 1;
        guard::Verdict {
            value,
            blocked: value > 10,
        }
    }
}
