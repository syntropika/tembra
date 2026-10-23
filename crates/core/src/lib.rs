//! A platform-neutral host for managed objects and late-bound references.
//!
//! Native values retain Rust identity across checked local calls. Refused argument types do
//! not invoke the handler; errors from an invoked handler can follow external effects.
//!
//! ```
//! use std::{cell::Cell, collections::BTreeMap, rc::Rc};
//! use futures::FutureExt;
//! use serde_json::json;
//! use tembra_core::*;
//! use contracts::{Contract, Member, ValueSchema};
//!
//! let driver = ManualDriver::new();
//! let host = Host::new(HostOptions::new("example", json!("host")), driver.clone());
//! let contract = Rc::new(Contract::local("sum", 1, json!({}), BTreeMap::from([
//!     ("add".into(), Member::Method {
//!         args: Some(ValueSchema::Native("example.amount@1".into())),
//!         result: Some(ValueSchema::Native("example.total@1".into())),
//!     }),
//! ]))?);
//! let total = Rc::new(Cell::new(0_i32));
//! let changed = total.clone();
//! host.serve(contract.clone(), Rc::new(move |_, _, _| {
//!     let changed = changed.clone();
//!     Ok(Implementation::new().method("add", move |amount: i32, _| {
//!         changed.set(changed.get() + amount);
//!         let result = changed.get();
//!         async move { Ok(result) }.boxed_local()
//!     }))
//! }), ServeOptions::default())?;
//! driver.run_until(async {
//!     let reference = host.get(contract, json!({}), GetOptions::default()).await?;
//!     assert_eq!(reference.call::<i32, i32>("add", 3, None).await?, 3);
//!     let error = reference.call::<u8, i32>("add", 9, None).await.unwrap_err();
//!     assert_eq!(error.reason, "native_type");
//!     assert_eq!(total.get(), 3); // Refusal did not invoke the handler.
//!     assert_eq!(reference.call::<i32, i32>("add", 0, None).await?, 3);
//!     reference.release();
//!     reference.release(); // Repeated release has no additional effect.
//!     assert_eq!(reference.call::<i32, i32>("add", 1, None).await.unwrap_err().code,
//!                ErrorCode::Released);
//!     assert_eq!(total.get(), 3);
//!     host.close().await
//! })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A retained context clone expires when its owning root settles.
//!
//! ```
//! use futures::FutureExt;
//! use serde_json::json;
//! use tembra_core::*;
//! let driver = ManualDriver::new();
//! let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
//! let context = driver.run_until(host.root(|ctx| async move {
//!     assert!(ctx.check(ctx.host_id()).is_ok());
//!     Ok(ctx)
//! }.boxed_local()))?;
//! assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
//! driver.run_until(host.close())?;
//! # Ok::<(), RuntimeError>(())
//! ```

pub mod activity;
pub mod context;
pub mod delta;
pub mod driver;
mod epoch;
pub mod error;
pub mod facet;
pub mod host;
pub mod invocation;
pub mod object;
pub mod observability;
pub mod reference;
pub mod registry;
pub mod resource;
pub mod rpc;
pub mod state;

pub use activity::*;
pub use context::{CancellationToken, Context, ScopeGuard, SettleSubscription};
pub use driver::{Driver, ManualDriver};
pub use error::{ErrorCode, Result, RuntimeError};
pub use facet::*;
pub use host::*;
pub use invocation::{InvocationLease, InvocationOwner, InvocationReceipt, ManagedCall};
pub use object::*;
pub use observability::{Observation, ObservationOptions, ObservationStatistics, Observations};
pub use reference::*;
pub use registry::*;
pub use resource::*;
pub use state::{
    Cell, Prepared, Readable, ReplaceOptions, Snapshot, StateDeclaration, StateDefinition,
    StateEvent, StateMetadataChange, Subscription,
};
pub use tembra_contracts as contracts;
