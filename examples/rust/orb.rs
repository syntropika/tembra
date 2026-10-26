//! Declare a counter with checked state changes.

use tembra::{Error, Orb, Service};

pub const COUNTER: &str = "example.counter";
pub const SET: &str = "set";
pub const READ: &str = "read";

/// Keep validation beside the service that owns the state.
pub fn counter() -> Orb {
    // 1. Declare one state value for each counter object.
    let service = Service::new(COUNTER, |declaration| {
        declaration.state(0_u32, 1, |value| {
            // 2. Reject invalid candidates without changing the current state.
            if *value <= 100 {
                Ok(())
            } else {
                Err(Error::invalid("counter_range", "count must be at most 100"))
            }
        })
    })
    // 3. Write through the method's admitted context.
    .method(SET, |state, context, value: u32| async move {
        Ok(*state.update(&context, |_| Ok(value))?)
    })
    // 4. Expose a read method without granting write access.
    .method(READ, |state, _, (): ()| async move { Ok(*state.value()?) });

    // 5. Package the service in a named, versioned orb.
    Orb::new(COUNTER, 1).service(service)
}
