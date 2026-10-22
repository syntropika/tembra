//! Typed, bounded hooks over the existing Host, registry and invocation ownership model.
//! Native values retain their real types; failures after user code do not undo its effects.
//!
//! A run snapshots one managed registry plan. The existing Driver owns started work
//! independently from the result observer, including physical invocation receipts.
//! Cancellation is a signal, so an uncooperative callback can keep cleanup pending.
//! Middleware requires [`hook_handler`] or [`hook_wrapper`] and explicit [`Next::recover`]
//! to acknowledge a failed continuation. Ordinary error catching does not acknowledge it.
//!
//! Empty series succeeds, but a retained expired Context cannot admit another run:
//!
//! ```
//! use futures::FutureExt;
//! use serde_json::json;
//! use std::{cell::RefCell, rc::Rc};
//! use tembra_core::{Host, HostOptions, LoadOptions, ManualDriver, NativeFacet};
//! use tembra_hooks::{HookDescriptor, HookManager, HookManagerOptions, HookType,
//!     RustHookDefinition, Strategy};
//! let driver = ManualDriver::new();
//! let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
//! let saved = Rc::new(RefCell::new(None));
//! let selected = saved.clone();
//! let startup = host.install(NativeFacet::new("hooks", vec![], move |scope, _| {
//!     *selected.borrow_mut() = Some(scope.clone());
//!     Ok(())
//! })?, LoadOptions::default())?;
//! driver.run_until(startup)?;
//! let manager = HookManager::new(saved.borrow_mut().take().unwrap(),
//!     driver.clone(), HookManagerOptions::default())?;
//! let input = HookType::<i32>::serde(json!({"type":"integer"}))?;
//! let descriptor = HookDescriptor::new("after-save", 1, Strategy::Series,
//!     input.profile().clone(), None, None)?;
//! let definition = RustHookDefinition::from_descriptor(descriptor, input, HookType::absent())?;
//! let (run_manager, run_definition) = (manager.clone(), definition.clone());
//! let expired = driver.run_until(host.root(move |context| async move {
//!     run_manager.run_dynamic_series(&run_definition, 3, &context).await?;
//!     Ok(context)
//! }.boxed_local()))?;
//! let refusal = driver.run_until(manager.run_dynamic_series(&definition, 3, &expired));
//! assert_eq!(refusal.unwrap_err().reason, "expired");
//! driver.run_until(host.close())?;
//! # Ok::<(), tembra_core::RuntimeError>(())
//! ```
mod definition;
mod descriptor;
mod manager;
mod next;
pub mod portable;
mod prepared;
mod wrapper;
pub use definition::{HookTerminal, HookType, RustHookDefinition};
pub use descriptor::{Descriptor, HookDescriptor, Strategy, TerminalProfile};
pub use manager::{BailDecision, BailOutcome, HookManager, HookManagerOptions, HookRun};
pub use next::{HookArgs, Next, OutputCompletion, ProjectedCall};
pub use prepared::{
    PreparedBail, PreparedMiddleware, PreparedParallel, PreparedSeries, PreparedWaterfall,
};
pub use wrapper::{MethodHandler, MethodWrapper, hook_handler, hook_wrapper};
pub(crate) fn invalid(reason: &str, message: impl Into<String>) -> tembra_core::RuntimeError {
    tembra_core::RuntimeError::invalid(reason, message)
}
