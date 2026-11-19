//! A fixed-WIT declaration shared by native and compiled hook matrix tests.
#[tembra::orb(
    package = "example:hook-gate@1.0.0",
    wit = "../runtime-fixture/admission"
)]
pub mod hook_gate {
    /// Contributors transform each admission in plan order.
    #[hook("example:admission/guard@1.0.0", composition = waterfall)]
    pub struct Guard;

    /// The service asks the same signed plan in either executor.
    #[service]
    pub struct Gate;

    impl Gate {
        /// Compose the guard while retaining the synchronous callback contract.
        pub async fn admit(value: i32) -> (i32, bool) {
            let answer = Guard::run(guard::Admission {
                value,
                blocked: false,
            })
            .await;
            (answer.value, answer.blocked)
        }
    }
}
