//! The native provider: a trusted Orb declared with the `tembra` facade. It declares the
//! waterfall `guard` hook with the WIT callback signature, so a compiled contributor is checked
//! against the same contract when it is installed, and keeps a ledger of the admissions it decided.
//!
//! A compiled Orb can declare a hook too, see `examples/rust-orb-hook-declaration`; this example
//! keeps the provider native to show a sandboxed contributor plugging into application code.
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};
use tembra::contracts::Contract;
use tembra::{Hook, Orb, Service};

/// The hook identity: the `guard` interface of the shared WIT package, which the contributor's
/// `#[hook]` attribute names too.
pub const GUARD: &str = "example:admission/guard@1.0.0";

/// The hook payload: the WIT record `admission`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Admission {
    pub value: i32,
    pub blocked: bool,
}

/// The provider's ledger: what passed, and how many admissions were blocked.
#[derive(Clone, Default, Serialize)]
pub struct Ledger {
    pub admitted: Vec<i32>,
    pub blocked: u32,
}

/// The `gate` Orb: the `guard` hook and a `gate` service whose `admit(value)` runs the hook and
/// records the decision, and whose `ledger()` reports what was decided so far.
pub fn orb() -> Result<Orb, Box<dyn std::error::Error>> {
    // The hook carries the WIT callback signature, read from the shared package.
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../wit"),
        Some("provider"),
    )?;
    let guard = Hook::<Admission, Admission>::waterfall_until(GUARD, |input| input.blocked)
        .signature(contract.function_signature(GUARD, "apply", true)?);
    let bound = guard.clone();
    let ledger = Rc::new(RefCell::new(Ledger::default()));
    Ok(Orb::new("gate", 1).hook(guard).service(
        Service::new("gate", move |declaration| {
            Ok((declaration.hook(&bound)?, ledger.clone()))
        })
        .method("admit", |gate, context, value: i32| async move {
            let input = Admission {
                value,
                blocked: false,
            };
            let decision = gate.0.run(&context, input).await?;
            let mut ledger = gate.1.borrow_mut();
            if decision.blocked {
                ledger.blocked += 1;
            } else {
                ledger.admitted.push(decision.value);
            }
            Ok(decision)
        })
        .method("ledger", |gate, _, (): ()| async move {
            Ok(gate.1.borrow().clone())
        }),
    ))
}
