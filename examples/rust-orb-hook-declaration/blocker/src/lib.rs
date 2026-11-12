//! A compiled contributor with nothing but its callback: it joins the `guard` hook that `gate`
//! declares and blocks negative values. The hook's interface is read from the contract the gate's
//! build wrote, as any interface of another Orb. An Orb needs no service to contribute.
#![cfg_attr(target_arch = "wasm32", no_std)]

#[tembra_orb_macro::orb(no_std, package = "example:blocker@1.0.0", wit = "../gate/contract")]
mod blocker {
    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:gate/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        if input.value < 0 {
            return guard::Admission {
                value: input.value,
                blocked: true,
            };
        }
        input
    }
}
