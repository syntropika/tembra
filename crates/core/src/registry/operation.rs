use super::{RegistryStore, superseded};
use crate::{
    Context, Reference, Result, RuntimeError,
    host::HostInner,
    object::{ObjectRecord, Phase},
};
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::Value;
use std::rc::{Rc, Weak};
use tembra_contracts::Contract;

pub(super) use crate::epoch::EpochRecord as Epoch;

/// A version-bound owner method. It holds no strong object/factory/resource reference.
/// Each call requires a live Context of this Host and exact execution version; native
/// calls preserve values and use the existing checked native dispatch channel.
/// A snapshot is not an already-held reference claim: starting a new call while the
/// Host is closing is refused. Calls admitted earlier retain their invocation pins.
#[derive(Clone)]
pub struct RegisteredOperation {
    pub(super) host: Weak<HostInner>,
    pub(super) owner: Weak<ObjectRecord>,
    pub(super) epoch: Weak<Epoch>,
    pub(super) store: Weak<RegistryStore>,
    pub(super) contract: Rc<Contract>,
    pub(super) method: String,
}

impl RegisteredOperation {
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Reads immutable method metadata for selecting an explicit language adapter.
    /// This observation creates no claim or dispatch authority; calls still check their Context.
    pub fn contract(&self) -> &Rc<Contract> {
        &self.contract
    }

    /// Compares immutable managed target identity for adapter-cache reuse.
    ///
    /// This creates no claim and performs no admission. Equal targets can both be retired;
    /// every invocation must still pass its ordinary Context, owner and epoch checks.
    pub fn same_target(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.host, &other.host)
            && Weak::ptr_eq(&self.owner, &other.owner)
            && Weak::ptr_eq(&self.epoch, &other.epoch)
            && self.method == other.method
            && self.contract.fingerprint() == other.contract.fingerprint()
    }

    /// Reports whether the weak target currently remains eligible for new acquisition.
    ///
    /// This is a cache-pruning hint, not dispatch permission or a retained lease. Context
    /// authority is intentionally not checked here; calls always recheck normal admission.
    pub fn is_live_target(&self) -> bool {
        let (Some(host), Some(owner), Some(epoch), Some(store)) = (
            self.host.upgrade(),
            self.owner.upgrade(),
            self.epoch.upgrade(),
            self.store.upgrade(),
        ) else {
            return false;
        };
        !host.is_closing()
            && owner.phase.get() == Phase::Live
            && epoch.admitted.get()
            && store.check_host().is_ok()
    }

    /// Calls through existing native dispatch, including Rust type checks and invocation
    /// pins. Cancellation does not undo effects; dropping the future follows Reference's
    /// local-call lifetime contract. No new root or implicit authority is created.
    ///
    /// # Errors
    /// Refuses invalid contexts, closing/closed Hosts, retired/mismatched versions,
    /// disposed owners, factory-time access and argument/result type mismatches before a handler.
    pub fn call<A: 'static, R: 'static>(
        &self,
        context: &Context,
        args: A,
    ) -> LocalBoxFuture<'static, Result<R>> {
        let operation = self.clone();
        let context = context.clone();
        async move {
            let reference = operation.reference(&context)?;
            reference
                .call(&operation.method, args, Some(&context))
                .await
        }
        .boxed_local()
    }

    /// Checks the actual native channel and Rust signature without entering user code.
    ///
    /// # Errors
    /// Refuses Context/owner/epoch/Host admission failures, factory access, missing methods
    /// and incompatible actual Rust argument/result types. No adapter or handler is called.
    pub fn check_native<A: 'static, R: 'static>(&self, context: &Context) -> Result<()> {
        let owner = self.owner_for(context)?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        if host.in_factory.get() > 0 {
            return Err(RuntimeError::new(
                crate::ErrorCode::NotReady,
                "factory",
                "native preflight is refused inside factories",
            ));
        }
        let implementation = owner
            .installed
            .borrow()
            .get(&context.code_version())
            .map(|installed| installed.implementation.clone())
            .ok_or_else(superseded)?;
        let member = self
            .contract
            .members("")
            .and_then(|members| members.get(&self.method));
        let returns = match member {
            Some(tembra_contracts::Member::Method { result, .. }) => result.is_some(),
            _ => {
                return Err(RuntimeError::invalid(
                    "native_type",
                    "registered member is not a method",
                ));
            }
        };
        let method = implementation.methods.get(&self.method).ok_or_else(|| {
            RuntimeError::invalid("native_type", "registered method has no native channel")
        })?;
        if method.args != std::any::TypeId::of::<A>()
            || (returns && method.result != std::any::TypeId::of::<R>())
            || (!returns && std::any::TypeId::of::<R>() != std::any::TypeId::of::<()>())
        {
            return Err(RuntimeError::invalid(
                "native_type",
                "registered method Rust signature differs",
            ));
        }
        Ok(())
    }

    /// Creates an opt-in tracked native call; construction invokes no handler.
    ///
    /// # Errors
    /// On polling, has ordinary `call` admission/result errors. Dropped or refused calls
    /// that never enter complete their receipt; entered physical leases remain observable.
    pub fn call_tracked<A: 'static, R: 'static>(
        &self,
        context: &Context,
        args: A,
    ) -> crate::ManagedCall<R> {
        let receipt = crate::InvocationReceipt::new();
        let observed = receipt.clone();
        let operation = self.clone();
        let context = context.clone();
        crate::ManagedCall::new(
            receipt,
            async move {
                let reference = operation.reference(&context)?;
                reference
                    .call_with_receipt(&operation.method, args, Some(&context), Some(&observed))
                    .await
            }
            .boxed_local(),
        )
    }

    /// Calls through existing portable dispatch; absence and explicit JSON null remain
    /// distinct. Argument/result schemas and invocation ownership are not bypassed.
    ///
    /// # Errors
    /// Has the same admission failures as `call`, plus portable binding/schema failures.
    /// A handler error can follow external effects; publication is not rolled back.
    pub fn call_json(
        &self,
        context: &Context,
        args: Option<Value>,
    ) -> LocalBoxFuture<'static, Result<Option<Value>>> {
        let operation = self.clone();
        let context = context.clone();
        async move {
            let reference = operation.reference(&context)?;
            reference
                .call_json(&operation.method, args, Some(&context))
                .await
        }
        .boxed_local()
    }

    fn reference(&self, context: &Context) -> Result<Reference> {
        let owner = self.owner_for(context)?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        Ok(Reference::fixed(
            &host,
            owner,
            self.contract.clone(),
            false,
            context.code_version(),
        ))
    }

    fn owner_for(&self, context: &Context) -> Result<Rc<ObjectRecord>> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "host no longer exists"))?;
        context.check(host.id)?;
        if host.is_closing() {
            return Err(RuntimeError::unavailable("closed", "host is closing"));
        }
        let epoch = self.epoch.upgrade().ok_or_else(super::superseded)?;
        if !epoch.admitted.get() || context.code_version() != epoch.version.get() {
            return Err(superseded());
        }
        self.store
            .upgrade()
            .ok_or_else(super::superseded)?
            .check_host()?;
        let owner = self.owner.upgrade().ok_or_else(|| {
            RuntimeError::new(
                crate::ErrorCode::Gone,
                "registry_owner",
                "registration owner was disposed",
            )
        })?;
        if owner.phase.get() != Phase::Live {
            return Err(RuntimeError::new(
                crate::ErrorCode::Gone,
                "registry_owner",
                "registration owner is not live",
            ));
        }
        Ok(owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ConflictPolicy, Host, HostOptions, Implementation, Lifetime, ManualDriver, Registry,
        ServeOptions,
    };
    use serde_json::json;
    use std::{cell::Cell, collections::BTreeMap};
    use tembra_contracts::Member;

    #[test]
    fn exact_epoch_and_retirement_refuse_before_native_handler() {
        let driver = ManualDriver::new();
        let host = Host::new(
            HostOptions::new("epoch-operation", Value::Null),
            driver.clone(),
        );
        let registry = Registry::new("ops", ConflictPolicy::Error).unwrap();
        let view = host.registry(&registry).unwrap();
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        host.serve(
            Rc::new(
                Contract::local(
                    "registry.epoch-op",
                    1,
                    json!({}),
                    BTreeMap::from([(
                        "run".into(),
                        Member::Method {
                            args: None,
                            result: None,
                        },
                    )]),
                )
                .unwrap(),
            ),
            Rc::new(move |_, _, declaration| {
                declaration.register_method(&registry, "run", Value::Null, "run")?;
                let calls = observed.clone();
                Ok(Implementation::new().method("run", move |(): (), _| {
                    calls.set(calls.get() + 1);
                    async { Ok(()) }.boxed_local()
                }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();
        driver.run_until(host.ready()).unwrap();
        let operation = view
            .table(None)
            .unwrap()
            .get("run")
            .unwrap()
            .operation()
            .unwrap()
            .clone();
        let (wrong, _wrong_scope) = Context::new_root(host.inner.id, 90, 2, Value::Null);
        assert_eq!(
            driver
                .run_until(operation.call::<(), ()>(&wrong, ()))
                .unwrap_err()
                .reason,
            "code_version"
        );
        operation.epoch.upgrade().unwrap().admitted.set(false);
        let (retired, _retired_scope) = Context::new_root(host.inner.id, 91, 1, Value::Null);
        assert_eq!(
            driver
                .run_until(operation.call::<(), ()>(&retired, ()))
                .unwrap_err()
                .reason,
            "code_version"
        );
        assert_eq!(calls.get(), 0);
        // Retirement is a private fixture only. Restore admission for ordinary Host cleanup.
        operation.epoch.upgrade().unwrap().admitted.set(true);
        driver.run_until(host.close()).unwrap();
    }
}
