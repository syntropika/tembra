//! Async imports return scalar tickets; payload allocation happens in a separate synchronous call.
use super::runtime::State;
use wasmtime::{StoreContextMut, component::Linker};

/// Retain data only within the callback that requested it, without exporting a kernel capability.
pub(super) fn create(
    state: &mut State,
    result: std::result::Result<String, String>,
) -> wasmtime::Result<u32> {
    if state.events.is_none() {
        return Ok(0);
    }
    let ticket = state
        .next_response
        .checked_add(1)
        .ok_or_else(|| wasmtime::Error::msg("response ticket space exhausted"))?;
    state.next_response = ticket;
    state.responses.insert(ticket, result);
    Ok(ticket)
}

/// Taking a ticket is single-use and cannot observe a response from a completed callback.
pub(super) fn add_to_linker(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    linker.instance("tembra:runtime/replies@0.3.0")?.func_wrap(
        "take",
        |mut store: StoreContextMut<State>, (ticket,): (u32,)| {
            let result = store
                .data_mut()
                .responses
                .remove(&ticket)
                .unwrap_or_else(|| Err("unknown or expired response ticket".into()));
            Ok((result,))
        },
    )?;
    Ok(())
}
