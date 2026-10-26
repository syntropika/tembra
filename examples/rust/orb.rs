//! Declare a counter with checked state changes.

use tembra::{Error, Orb, Service};

pub const COUNTER: &str = "example.counter";
pub const SET: &str = "set";
pub const READ: &str = "read";

/// Keep validation beside the service that owns the state.
pub fn counter() -> Orb {
    let service = Service::new(COUNTER, |declaration| {
        declaration.state(0_u32, 1, |value| {
            if *value <= 100 {
                Ok(())
            } else {
                Err(Error::invalid("counter_range", "count must be at most 100"))
            }
        })
    })
    .method(SET, |state, context, value: u32| async move {
        Ok(*state.update(&context, |_| Ok(value))?)
    })
    .method(READ, |state, _, (): ()| async move { Ok(*state.value()?) });

    Orb::new(COUNTER, 1).service(service)
}
