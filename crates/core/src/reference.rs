use crate::state::ReadPort;
use crate::{
    Context, ErrorCode, Readable, Result, RuntimeError, Snapshot, StateEvent, Subscription,
};
use crate::{
    host::HostInner,
    object::{ObjectRecord, Phase},
};
use futures::{channel::oneshot, future::FutureExt};
use std::{
    any::TypeId,
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefStatus {
    Loading,
    Live,
    Released,
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub variant: Option<String>,
    pub members: Vec<String>,
    pub contract: String,
}

type StatusListener = Rc<dyn Fn(RefStatus, Option<RuntimeError>)>;
type ShapeListener = Rc<dyn Fn(Shape)>;

pub(crate) enum ReferenceBacking {
    Local(Rc<ObjectRecord>),
    Remote(Rc<crate::rpc::remote::RemoteBinding>),
}
#[derive(Clone, Copy)]
enum BindingTarget {
    Current,
    Fixed(u64),
    Pending(u64),
}

pub(crate) struct Claim {
    pub host: Weak<HostInner>,
    pub backing: ReferenceBacking,
    pub contract: Rc<tembra_contracts::Contract>,
    pub object_owned: Cell<bool>,
    binding_target: Cell<BindingTarget>,
    ended: Cell<Option<RefStatus>>,
    error: RefCell<Option<RuntimeError>>,
    listeners: RefCell<Vec<StatusListener>>,
    shape_listeners: RefCell<Vec<ShapeListener>>,
    last_shape: RefCell<Option<Shape>>,
    pub(crate) wants_state: bool,
    state_interest: Cell<usize>,
    state_started: Cell<bool>,
    state_ready: Cell<bool>,
    state_cancel: RefCell<crate::CancellationToken>,
    waiters: RefCell<HashMap<u64, oneshot::Sender<Result<()>>>>,
    binding_started: Cell<bool>,
    binding_scheduled: Cell<bool>,
    binding_setup: Cell<bool>,
    binding_live: Cell<bool>,
    required_states: RefCell<Vec<String>>,
    binding_values: RefCell<HashMap<String, Snapshot>>,
    proxy_ports: RefCell<HashMap<String, Rc<ReadPort>>>,
    proxy_subscriptions: RefCell<Vec<Subscription>>,
    next_waiter: Cell<u64>,
    notified: Cell<RefStatus>,
    reload_publication_paused: Cell<bool>,
    reload_ready_pending: Cell<bool>,
}

impl Claim {
    pub(crate) fn binding_ended(&self) -> bool {
        self.ended.get().is_some()
    }
    pub(crate) fn projection_links(
        &self,
        view: &crate::Implementation,
    ) -> Vec<(Readable, Readable)> {
        self.proxy_ports
            .borrow()
            .iter()
            .filter_map(|(name, port)| {
                view.states
                    .get(name)
                    .map(|source| (port.readable(), source.clone()))
            })
            .collect()
    }
    pub(crate) fn fixed_binding(&self) -> bool {
        matches!(self.binding_target.get(), BindingTarget::Fixed(_))
    }
    pub(crate) fn freeze_binding(&self, version: u64) {
        self.binding_target.set(BindingTarget::Fixed(version));
    }
    pub(crate) fn publish_binding(&self) {
        if matches!(self.binding_target.get(), BindingTarget::Pending(_)) {
            self.binding_target.set(BindingTarget::Current);
        }
    }
    fn binding_version(&self) -> Result<u64> {
        match self.binding_target.get() {
            BindingTarget::Current => Ok(self.local()?.current_epoch.get()),
            BindingTarget::Fixed(version) | BindingTarget::Pending(version) => Ok(version),
        }
    }
    pub(crate) fn local_object(&self) -> Option<&Rc<ObjectRecord>> {
        match &self.backing {
            ReferenceBacking::Local(object) => Some(object),
            ReferenceBacking::Remote(_) => None,
        }
    }
    fn local(&self) -> Result<&Rc<ObjectRecord>> {
        self.local_object().ok_or_else(|| {
            RuntimeError::invalid(
                "native_type",
                "remote references have no native implementation",
            )
        })
    }
    fn backing_error(&self) -> Option<RuntimeError> {
        match &self.backing {
            ReferenceBacking::Local(object) => object.error.borrow().clone(),
            ReferenceBacking::Remote(remote) => remote.error(),
        }
    }
    pub(crate) fn status_for_remote(&self) -> RefStatus {
        self.ended.get().unwrap_or(RefStatus::Loading)
    }
    pub(crate) fn state_wanted(&self) -> bool {
        self.wants_state || self.state_interest.get() > 0
    }

    pub fn notify(self: &Rc<Self>) {
        if let ReferenceBacking::Remote(remote) = &self.backing {
            remote.sync_claim(self);
            return;
        }
        if self.ended.get().is_none() {
            let object = self.local_object().expect("local backing");
            match object.phase.get() {
                Phase::Live => self.start_binding(),
                Phase::Activating => {}
                _ => {
                    let error = object.error.borrow().clone().unwrap_or_else(|| {
                        RuntimeError::new(ErrorCode::Gone, "gone", "object is gone")
                    });
                    self.mark_gone(error);
                    return;
                }
            }
        }
        self.notify_status();
    }

    fn notify_status(&self) {
        if self.reload_publication_paused.get() {
            return;
        }
        let status = self.status();
        if self.notified.replace(status) == status {
            return;
        }
        let error = self.error.borrow().clone().or_else(|| self.backing_error());
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            if self.notified.get() != status {
                break;
            }
            if self
                .listeners
                .borrow()
                .iter()
                .any(|stored| Rc::ptr_eq(stored, &listener))
            {
                listener(status, error.clone());
            }
        }
    }

    fn status(&self) -> RefStatus {
        self.ended.get().unwrap_or_else(|| match &self.backing {
            ReferenceBacking::Remote(remote) => remote.claim_status(self),
            ReferenceBacking::Local(object) => match object.phase.get() {
                Phase::Activating => RefStatus::Loading,
                Phase::Live if self.binding_live.get() => RefStatus::Live,
                Phase::Live => RefStatus::Loading,
                _ => RefStatus::Gone,
            },
        })
    }

    fn release(&self) {
        if self.ended.get().is_some() {
            return;
        }
        self.ended.set(Some(RefStatus::Released));
        let error = RuntimeError::new(ErrorCode::Released, "released", "reference released");
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters.into_values() {
            let _ = waiter.send(Err(error.clone()));
        }
        if let Some(object) = self.local_object() {
            object
                .references
                .set(object.references.get().saturating_sub(1));
        }
        self.stop_proxies(error.clone());
        self.release_unused_adapter_view();
        self.notify_status();
        if !self.reload_publication_paused.get() {
            let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
            let shape_listeners = std::mem::take(&mut *self.shape_listeners.borrow_mut());
            drop((listeners, shape_listeners));
        }
        match &self.backing {
            ReferenceBacking::Local(object) => {
                if let Some(host) = self.host.upgrade() {
                    host.idle(object.clone());
                }
            }
            ReferenceBacking::Remote(remote) => remote.claim_released(),
        }
    }

    pub fn mark_gone(&self, error: RuntimeError) {
        if self.ended.get().is_some() {
            return;
        }
        self.ended.set(Some(RefStatus::Gone));
        *self.error.borrow_mut() = Some(error.clone());
        if let Some(object) = self.local_object() {
            object
                .references
                .set(object.references.get().saturating_sub(1));
        }
        self.stop_proxies(error.clone());
        self.release_unused_adapter_view();
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters.into_values() {
            let _ = waiter.send(Err(error.clone()));
        }
        self.notify_status();
        if !self.reload_publication_paused.get() {
            let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
            let shape_listeners = std::mem::take(&mut *self.shape_listeners.borrow_mut());
            drop((listeners, shape_listeners));
        }
        if let ReferenceBacking::Remote(remote) = &self.backing {
            remote.claim_released();
        }
    }

    pub(crate) fn reload_proxy_snapshots(
        self: &Rc<Self>,
        view: &crate::Implementation,
    ) -> Result<Vec<(Rc<ReadPort>, Snapshot)>> {
        if self.ended.get().is_some() {
            return Ok(Vec::new());
        }
        let existing = self
            .proxy_ports
            .borrow()
            .iter()
            .filter(|(_, port)| port.current().is_ok())
            .map(|(name, port)| (name.clone(), port.clone()))
            .collect::<Vec<_>>();
        existing
            .into_iter()
            .filter_map(|(name, port)| {
                view.states
                    .get(&name)
                    .map(|source| source.snapshot().map(|snapshot| (port, snapshot)))
            })
            .collect()
    }
    pub(crate) fn begin_reload_publication(&self) {
        self.reload_publication_paused.set(true);
    }
    pub(crate) fn finish_reload_publication(self: &Rc<Self>) {
        self.reload_publication_paused.set(false);
        if self.reload_ready_pending.replace(false) {
            self.publish_ready();
        } else {
            self.notify_status();
        }
        if self.ended.get().is_some() {
            let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
            let shape_listeners = std::mem::take(&mut *self.shape_listeners.borrow_mut());
            drop((listeners, shape_listeners));
        }
    }
    pub(crate) fn refresh_local_binding(self: &Rc<Self>) {
        if self.ended.get().is_some() {
            return;
        }
        let cancellation = self.state_cancel.replace(crate::CancellationToken::new());
        let subscriptions = std::mem::take(&mut *self.proxy_subscriptions.borrow_mut());
        let values = std::mem::take(&mut *self.binding_values.borrow_mut());
        self.binding_started.set(false);
        self.state_started.set(false);
        self.state_ready.set(false);
        self.binding_setup.set(false);
        self.binding_live.set(false);
        cancellation.cancel(RuntimeError::aborted(
            "code_version",
            "reference selected a newer installed view",
        ));
        drop((subscriptions, values));
        self.notify();
    }

    fn stop_proxies(&self, error: RuntimeError) {
        let cancellation = self.state_cancel.borrow().clone();
        cancellation.cancel(error.clone());
        let subscriptions = std::mem::take(&mut *self.proxy_subscriptions.borrow_mut());
        drop(subscriptions);
        let initial_values = std::mem::take(&mut *self.binding_values.borrow_mut());
        drop(initial_values);
        let ports: Vec<_> = self.proxy_ports.borrow().values().cloned().collect();
        for port in ports {
            port.terminate(error.clone());
        }
    }

    fn proxy_port(self: &Rc<Self>, name: &str) -> Result<Rc<ReadPort>> {
        if let Some(port) = self.proxy_ports.borrow().get(name).cloned() {
            return Ok(port);
        }
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        let weak = Rc::downgrade(self);
        let member = name.to_owned();
        let read = Rc::new(move || {
            let claim = weak.upgrade().ok_or_else(|| {
                RuntimeError::new(ErrorCode::Gone, "reference", "reference no longer exists")
            })?;
            claim.check()?;
            if let Some(shape) = (Reference {
                claim: claim.clone(),
            })
            .shape()
                && !matches!(
                    claim
                        .contract
                        .members(shape.variant.as_deref().unwrap_or(""))
                        .and_then(|members| members.get(&member)),
                    Some(tembra_contracts::Member::State { .. })
                )
            {
                return Err(RuntimeError::new(
                    ErrorCode::NotInVariant,
                    "state",
                    "state is absent from the selected variant",
                ));
            }
            Ok(())
        });
        let weak = Rc::downgrade(self);
        let subscribe = Rc::new(move || {
            let claim = weak.upgrade().ok_or_else(|| {
                RuntimeError::new(ErrorCode::Gone, "reference", "reference no longer exists")
            })?;
            claim.check()?;
            if claim
                .host
                .upgrade()
                .is_some_and(|host| host.in_factory.get() > 0)
            {
                return Err(RuntimeError::invalid(
                    "read_only",
                    "state subscriptions cannot start inside a factory",
                ));
            }
            Ok(())
        });
        let reporting_host = Rc::downgrade(&host);
        let report = Rc::new(move |error| {
            if let Some(host) = reporting_host.upgrade() {
                host.errors.borrow_mut().push(error);
            }
        });
        let port = ReadPort::new(read, subscribe, report, host.publication_queue.clone());
        let weak = Rc::downgrade(self);
        port.set_interest_hook(Rc::new(move |wanted| {
            if let Some(claim) = weak.upgrade() {
                if wanted {
                    claim.state_interest.set(claim.state_interest.get() + 1);
                } else {
                    claim
                        .state_interest
                        .set(claim.state_interest.get().saturating_sub(1));
                }
                match &claim.backing {
                    ReferenceBacking::Remote(remote) => remote.interest_changed(),
                    ReferenceBacking::Local(_) => claim.local_interest_changed(),
                }
            } else {
                Ok(())
            }
        }));
        self.proxy_ports
            .borrow_mut()
            .insert(name.into(), port.clone());
        Ok(port)
    }

    fn start_binding(self: &Rc<Self>) {
        if self.binding_started.get() || self.ended.get().is_some() {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            return;
        };
        if let BindingTarget::Pending(version) = self.binding_target.get()
            && self
                .local_object()
                .is_some_and(|object| !object.installed.borrow().contains_key(&version))
        {
            return;
        }
        if host.in_factory.get() > 0 {
            if !self.binding_scheduled.replace(true) {
                let weak = Rc::downgrade(self);
                host.driver.spawn(
                    async move {
                        if let Some(claim) = weak.upgrade() {
                            claim.binding_scheduled.set(false);
                            claim.notify();
                        }
                    }
                    .boxed_local(),
                );
            }
            return;
        }
        self.binding_started.set(true);
        if let Err(error) = self.prepare_binding(&host) {
            self.mark_gone(error);
        }
    }

    fn prepare_binding(self: &Rc<Self>, host: &Rc<HostInner>) -> Result<()> {
        let reference = Reference {
            claim: self.clone(),
        };
        let view = reference.implementation_view()?;
        let variant = view.variant.as_deref().unwrap_or("");
        let members = self
            .contract
            .members(variant)
            .expect("validated consumer variant");
        let sources: Vec<_> = members
            .iter()
            .filter_map(|(name, member)| {
                if let tembra_contracts::Member::State { .. } = member {
                    Some((
                        name.clone(),
                        view.states[name].clone(),
                        (self.contract.clone(), variant.to_owned()),
                    ))
                } else {
                    None
                }
            })
            .collect();
        drop(view);
        *self.required_states.borrow_mut() =
            sources.iter().map(|(name, _, _)| name.clone()).collect();
        for (name, _, _) in &sources {
            self.proxy_port(name)?;
        }
        if !self.state_wanted() {
            self.binding_setup.set(true);
            self.binding_live.set(true);
            self.publish_ready();
            return Ok(());
        }
        self.state_started.set(true);
        let cancellation = crate::CancellationToken::new();
        *self.state_cancel.borrow_mut() = cancellation.clone();
        for (name, source, schema) in &sources {
            let weak = Rc::downgrade(self);
            let name = name.clone();
            let schema = schema.clone();
            let subscription = source.subscribe_events(move |event| {
                let Some(claim) = weak.upgrade() else {
                    return Ok(());
                };
                let result = claim.receive_state(&name, &schema, event);
                if let Err(error) = &result
                    && !claim.state_ready.get()
                {
                    claim.mark_gone(error.clone());
                }
                result
            })?;
            self.check()?;
            self.proxy_subscriptions.borrow_mut().push(subscription);
        }
        self.binding_setup.set(true);
        self.finish_binding()?;
        if !self.state_ready.get() {
            let weak = Rc::downgrade(self);
            host.driver.spawn(
                async move {
                    if cancellation.is_cancelled() {
                        return;
                    }
                    let waiting = futures::future::try_join_all(
                        sources.iter().map(|(_, source, _)| source.await_hydrated()),
                    )
                    .boxed_local();
                    let result =
                        match futures::future::select(waiting, cancellation.cancelled()).await {
                            futures::future::Either::Left((result, _)) => result,
                            futures::future::Either::Right(_) => return,
                        };
                    if cancellation.is_cancelled() {
                        return;
                    }
                    if let Some(claim) = weak.upgrade() {
                        if claim.ended.get().is_some() || claim.state_ready.get() {
                            return;
                        }
                        if let Err(error) = result {
                            claim.mark_gone(error);
                            return;
                        }
                        for (name, source, schema) in sources {
                            let result = source.snapshot().and_then(|snapshot| {
                                claim.receive_state(&name, &schema, StateEvent::Snapshot(snapshot))
                            });
                            if let Err(error) = result {
                                claim.mark_gone(error);
                                return;
                            }
                        }
                        if let Err(error) = claim.finish_binding() {
                            claim.mark_gone(error);
                        }
                    }
                }
                .boxed_local(),
            );
        }
        Ok(())
    }

    fn receive_state(
        self: &Rc<Self>,
        name: &str,
        schema: &(Rc<tembra_contracts::Contract>, String),
        event: StateEvent,
    ) -> Result<()> {
        self.check()?;
        schema
            .0
            .validate_state(&schema.1, name, event.snapshot().value())
            .map_err(|error| RuntimeError::invalid("state", error.message))?;
        if self.state_ready.get() {
            let host = self
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
            let event = match event {
                StateEvent::Snapshot(snapshot) => StateEvent::Reset(snapshot),
                event => event,
            };
            if let Some(publication) = self.proxy_port(name)?.adopt(event) {
                host.publication_queue.append_batch(vec![publication]);
            }
        } else {
            self.binding_values
                .borrow_mut()
                .insert(name.into(), event.snapshot().clone());
            if self.binding_setup.get() {
                self.finish_binding()?;
            }
        }
        Ok(())
    }

    fn finish_binding(self: &Rc<Self>) -> Result<()> {
        if self.state_ready.get() || !self.binding_setup.get() {
            return Ok(());
        }
        self.check()?;
        let names = self.required_states.borrow().clone();
        let values: Option<Vec<_>> = names
            .iter()
            .map(|name| {
                self.binding_values
                    .borrow()
                    .get(name)
                    .cloned()
                    .map(|snapshot| (name.clone(), snapshot))
            })
            .collect();
        let Some(values) = values else {
            return Ok(());
        };
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        let _hold = host.publication_queue.hold();
        let ports = values
            .into_iter()
            .map(|(name, snapshot)| self.proxy_port(&name).map(|port| (port, snapshot)))
            .collect::<Result<Vec<_>>>()?;
        if ports.iter().any(|(port, _)| port.is_terminated()) {
            return Err(RuntimeError::new(
                ErrorCode::Gone,
                "state",
                "binding state port terminated",
            ));
        }
        let publications = ports
            .into_iter()
            .filter_map(|(port, snapshot)| {
                let event = if port.current().is_ok() {
                    StateEvent::Reset(snapshot)
                } else {
                    StateEvent::Snapshot(snapshot)
                };
                port.adopt(event)
            })
            .collect();
        host.publication_queue.append_batch(publications);
        self.state_ready.set(true);
        self.binding_live.set(true);
        let initial_values = std::mem::take(&mut *self.binding_values.borrow_mut());
        drop(initial_values);
        let unused: Vec<_> = self
            .proxy_ports
            .borrow()
            .iter()
            .filter(|(name, _)| !names.contains(name))
            .map(|(_, port)| port.clone())
            .collect();
        for port in unused {
            port.terminate(RuntimeError::new(
                ErrorCode::NotInVariant,
                "state",
                "state is absent from selected variant",
            ));
        }
        self.publish_ready();
        Ok(())
    }

    fn publish_ready(self: &Rc<Self>) {
        if self.reload_publication_paused.get() {
            self.reload_ready_pending.set(true);
            return;
        }
        let reference = Reference {
            claim: self.clone(),
        };
        if let Some(shape) = reference.shape() {
            let changed = self.last_shape.borrow().as_ref() != Some(&shape);
            *self.last_shape.borrow_mut() = Some(shape.clone());
            if changed {
                let listeners = self.shape_listeners.borrow().clone();
                for listener in listeners {
                    if self.ended.get().is_some()
                        || self.last_shape.borrow().as_ref() != Some(&shape)
                    {
                        break;
                    }
                    if self
                        .shape_listeners
                        .borrow()
                        .iter()
                        .any(|stored| Rc::ptr_eq(stored, &listener))
                    {
                        listener(shape.clone());
                    }
                }
            }
        }
        self.notify_status();
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters.into_values() {
            let _ = waiter.send(Ok(()));
        }
    }

    fn local_interest_changed(self: &Rc<Self>) -> Result<()> {
        if self.ended.get().is_some() {
            return Ok(());
        }
        if self.state_wanted() && self.binding_started.get() && !self.state_started.get() {
            let host = self
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host ended"))?;
            self.prepare_binding(&host)?;
        } else if !self.state_wanted() && self.state_started.replace(false) {
            let cancellation = self.state_cancel.borrow().clone();
            cancellation.cancel(RuntimeError::aborted(
                "state_interest",
                "state demand ended",
            ));
            let subscriptions = std::mem::take(&mut *self.proxy_subscriptions.borrow_mut());
            drop(subscriptions);
        }
        Ok(())
    }

    pub(crate) fn remote_end(&self, error: RuntimeError) {
        self.mark_gone(error);
    }
    pub(crate) fn remote_loading(&self) {
        self.binding_live.set(false);
        self.notify_status();
    }
    pub(crate) fn remote_candidates(
        self: &Rc<Self>,
        values: &[(String, StateEvent)],
    ) -> Result<Vec<(Rc<ReadPort>, StateEvent)>> {
        self.check()?;
        values
            .iter()
            .map(|(name, event)| self.proxy_port(name).map(|port| (port, event.clone())))
            .collect()
    }
    pub(crate) fn remote_set_ready(&self, ready: bool) {
        self.binding_live.set(ready);
    }
    pub(crate) fn remote_publish_ready(self: &Rc<Self>) {
        self.publish_ready();
    }

    fn check(&self) -> Result<()> {
        match self.status() {
            RefStatus::Released => Err(RuntimeError::new(
                ErrorCode::Released,
                "released",
                "reference released",
            )),
            RefStatus::Gone => Err(self
                .error
                .borrow()
                .clone()
                .or_else(|| self.backing_error())
                .unwrap_or_else(|| RuntimeError::new(ErrorCode::Gone, "gone", "object is gone"))),
            _ => self
                .host
                .upgrade()
                .map_or(Ok(()), |host| host.epochs.check()),
        }
    }

    fn check_method_publication(&self, host: &HostInner) -> Result<()> {
        if let BindingTarget::Pending(version) = self.binding_target.get()
            && self
                .local_object()
                .is_some_and(|object| !object.installed.borrow().contains_key(&version))
        {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "candidate_private",
                "pending candidate methods are unavailable before coherent publication",
            ));
        }
        if let Some(object) = self.local_object()
            && object.phase.get() == Phase::Activating
            && !host
                .objects
                .borrow()
                .get(&object.key)
                .is_some_and(|installed| Rc::ptr_eq(installed, object))
        {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "candidate_private",
                "new candidate methods are unavailable before coherent publication",
            ));
        }
        Ok(())
    }

    fn release_unused_adapter_view(&self) {
        let Some(object) = self.local_object() else {
            return;
        };
        let fingerprint = self.contract.fingerprint();
        let still_used = object
            .claims
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|claim| {
                claim.ended.get().is_none() && claim.contract.fingerprint() == fingerprint
            });
        if !still_used {
            let mut retired = Vec::new();
            let view = object.adapter_views.borrow_mut().remove(fingerprint);
            retired.extend(view);
            let versions = object
                .installed
                .borrow()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for version in versions {
                retired.extend(version.adapters.borrow_mut().remove(fingerprint));
            }
            drop(retired);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.release();
    }
}

/// Cloning shares this reference claim. Use another acquisition for an independent claim.
#[derive(Clone)]
pub struct Reference {
    pub(crate) claim: Rc<Claim>,
}

impl Reference {
    pub(crate) fn new(
        host: &Rc<HostInner>,
        object: Rc<ObjectRecord>,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
    ) -> Self {
        Self::new_with_target(host, object, contract, wants_state, BindingTarget::Current)
    }
    pub(crate) fn pending(
        host: &Rc<HostInner>,
        object: Rc<ObjectRecord>,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
        version: u64,
    ) -> Self {
        Self::new_with_target(
            host,
            object,
            contract,
            wants_state,
            BindingTarget::Pending(version),
        )
    }
    pub(crate) fn fixed(
        host: &Rc<HostInner>,
        object: Rc<ObjectRecord>,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
        version: u64,
    ) -> Self {
        Self::new_with_target(
            host,
            object,
            contract,
            wants_state,
            BindingTarget::Fixed(version),
        )
    }
    fn new_with_target(
        host: &Rc<HostInner>,
        object: Rc<ObjectRecord>,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
        target: BindingTarget,
    ) -> Self {
        object.references.set(object.references.get() + 1);
        let reference = Self::with_backing(
            host,
            ReferenceBacking::Local(object.clone()),
            contract,
            wants_state,
        );
        reference.claim.binding_target.set(target);
        let mut claims = object.claims.borrow_mut();
        claims.retain(|claim| claim.strong_count() > 0);
        claims.push(Rc::downgrade(&reference.claim));
        drop(claims);
        reference.claim.notify();
        reference
    }

    pub(crate) fn remote(
        host: &Rc<HostInner>,
        remote: Rc<crate::rpc::remote::RemoteBinding>,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
    ) -> Self {
        let reference = Self::with_backing(
            host,
            ReferenceBacking::Remote(remote.clone()),
            contract,
            wants_state,
        );
        remote.add_claim(&reference.claim);
        reference.claim.notify();
        reference
    }

    fn with_backing(
        host: &Rc<HostInner>,
        backing: ReferenceBacking,
        contract: Rc<tembra_contracts::Contract>,
        wants_state: bool,
    ) -> Self {
        let claim = Rc::new(Claim {
            host: Rc::downgrade(host),
            backing,
            contract,
            object_owned: Cell::new(false),
            binding_target: Cell::new(BindingTarget::Current),
            ended: Cell::new(None),
            error: RefCell::new(None),
            listeners: RefCell::new(Vec::new()),
            shape_listeners: RefCell::new(Vec::new()),
            last_shape: RefCell::new(None),
            wants_state,
            state_interest: Cell::new(0),
            state_started: Cell::new(false),
            state_ready: Cell::new(false),
            state_cancel: RefCell::new(crate::CancellationToken::new()),
            waiters: RefCell::new(HashMap::new()),
            next_waiter: Cell::new(0),
            notified: Cell::new(RefStatus::Loading),
            reload_publication_paused: Cell::new(false),
            reload_ready_pending: Cell::new(false),
            binding_started: Cell::new(false),
            binding_scheduled: Cell::new(false),
            binding_setup: Cell::new(false),
            binding_live: Cell::new(false),
            required_states: RefCell::new(Vec::new()),
            binding_values: RefCell::new(HashMap::new()),
            proxy_ports: RefCell::new(HashMap::new()),
            proxy_subscriptions: RefCell::new(Vec::new()),
        });
        Self { claim }
    }

    pub(crate) fn address(&self) -> crate::host::Address {
        match &self.claim.backing {
            ReferenceBacking::Local(object) => object.key.clone(),
            ReferenceBacking::Remote(remote) => crate::host::Address(
                self.contract().name().into(),
                self.contract()
                    .identity(&remote.input)
                    .expect("validated remote input"),
                format!("remote:{}", remote.target),
            ),
        }
    }

    pub fn object_id(&self) -> u64 {
        match &self.claim.backing {
            ReferenceBacking::Local(object) => object.id,
            ReferenceBacking::Remote(remote) => remote.id(),
        }
    }
    pub fn contract(&self) -> &Rc<tembra_contracts::Contract> {
        &self.claim.contract
    }
    pub fn status(&self) -> RefStatus {
        self.claim.status()
    }
    pub fn error(&self) -> Option<RuntimeError> {
        self.claim
            .error
            .borrow()
            .clone()
            .or_else(|| self.claim.backing_error())
    }
    /// Releases this claim and all its clones, exactly once.
    ///
    /// Active calls keep their invocation ownership; independent physical leases may retain it
    /// after a logical call settles or is dropped.
    /// This operation schedules eligible cleanup; it does not wait for resource closure.
    pub fn release(&self) {
        self.claim.release();
    }
    pub fn dependency(&self) -> crate::ResourceDependency {
        crate::ResourceDependency::Reference(self.clone())
    }
    pub(crate) fn same_dependency(&self, other: &Self) -> bool {
        if !Weak::ptr_eq(&self.claim.host, &other.claim.host)
            || self.contract().fingerprint() != other.contract().fingerprint()
            || self.claim.wants_state != other.claim.wants_state
        {
            return false;
        }
        match (&self.claim.backing, &other.claim.backing) {
            (ReferenceBacking::Local(a), ReferenceBacking::Local(b)) => Rc::ptr_eq(a, b),
            (ReferenceBacking::Remote(a), ReferenceBacking::Remote(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub(crate) fn same_claim(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.claim, &other.claim)
    }

    pub fn shape(&self) -> Option<Shape> {
        if let ReferenceBacking::Remote(remote) = &self.claim.backing {
            return remote.shape();
        }
        if self.status() != RefStatus::Live {
            return None;
        }
        let view = self.implementation_view().ok()?;
        let variant = view.variant.clone();
        Some(Shape {
            variant: variant.clone(),
            members: self
                .claim
                .contract
                .members(variant.as_deref().unwrap_or(""))
                .map(|members| members.keys().cloned().collect())
                .unwrap_or_default(),
            contract: {
                let object = self.claim.local_object()?;
                object
                    .installed
                    .borrow()
                    .get(&self.claim.binding_version().ok()?)?
                    .service
                    .contract()
                    .fingerprint()
                    .into()
            },
        })
    }

    /// Returns a stable weak readable for a state member, including while loading.
    ///
    /// Loading subscriptions receive the first validated snapshot when this complete
    /// consumer binding becomes live. Retaining a readable does not retain the claim.
    ///
    /// # Errors
    /// Refuses ended references or names that are not state in the consumer contract's
    /// selected variant (or any possible variant while loading).
    pub fn readable(&self, name: &str) -> Result<Readable> {
        self.claim.check()?;
        let is_state = if self.status() == RefStatus::Live {
            let shape = self.shape();
            matches!(
                self.contract()
                    .members(
                        shape
                            .as_ref()
                            .and_then(|shape| shape.variant.as_deref())
                            .unwrap_or("")
                    )
                    .and_then(|members| members.get(name)),
                Some(tembra_contracts::Member::State { .. })
            )
        } else {
            self.contract().variants().values().any(|members| {
                matches!(
                    members.get(name),
                    Some(tembra_contracts::Member::State { .. })
                )
            })
        };
        if !is_state {
            return Err(RuntimeError::new(
                ErrorCode::NotInVariant,
                "state",
                "state is not exposed by the consumer contract",
            ));
        }
        self.claim.proxy_port(name).map(|port| port.readable())
    }

    pub fn on_status(
        &self,
        listener: impl Fn(RefStatus, Option<RuntimeError>) + 'static,
    ) -> Box<dyn Fn()> {
        let listener: StatusListener = Rc::new(listener);
        self.claim.listeners.borrow_mut().push(listener.clone());
        let claim = Rc::downgrade(&self.claim);
        Box::new(move || {
            if let Some(claim) = claim.upgrade() {
                claim
                    .listeners
                    .borrow_mut()
                    .retain(|stored| !Rc::ptr_eq(stored, &listener));
            }
        })
    }

    /// Observes coherent shape changes. The returned remover does not retain this claim.
    pub fn on_shape(&self, listener: impl Fn(Shape) + 'static) -> Box<dyn Fn()> {
        let listener: ShapeListener = Rc::new(listener);
        self.claim
            .shape_listeners
            .borrow_mut()
            .push(listener.clone());
        if let Some(shape) = self.shape() {
            listener(shape);
        }
        let claim = Rc::downgrade(&self.claim);
        Box::new(move || {
            if let Some(claim) = claim.upgrade() {
                let removed = {
                    let mut listeners = claim.shape_listeners.borrow_mut();
                    listeners
                        .iter()
                        .position(|stored| Rc::ptr_eq(stored, &listener))
                        .map(|index| listeners.remove(index))
                };
                drop(removed);
            }
        })
    }

    pub(crate) fn rpc_shape_data(
        &self,
    ) -> Result<(
        Rc<tembra_contracts::Contract>,
        crate::rpc::protocol::BindingMode,
    )> {
        if let ReferenceBacking::Remote(remote) = &self.claim.backing {
            return remote.provider_view();
        }
        let object = self.claim.local()?;
        self.implementation_view()?;
        let provider = object
            .installed
            .borrow()
            .get(&self.claim.binding_version()?)
            .map(|installed| installed.service.contract().clone())
            .ok_or_else(crate::epoch::superseded)?;
        let mode = if provider.version() == self.contract().version() {
            crate::rpc::protocol::BindingMode::Direct
        } else {
            crate::rpc::protocol::BindingMode::Adapted {
                consumer_fingerprint: self.contract().fingerprint().into(),
            }
        };
        Ok((provider, mode))
    }

    /// Returns a checked native resource value exposed by the consumer contract.
    ///
    /// # Errors
    /// Refuses ended/loading claims, absent members, unopened resources and Rust type
    /// mismatches. A returned `Rc` preserves native identity and may outlive logical resource
    /// closure; retaining it does not keep the object active.
    pub fn opaque<T: 'static>(&self, name: &str) -> Result<Rc<T>> {
        self.claim.check()?;
        self.claim.local()?;
        if self.status() != RefStatus::Live {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "opaque",
                "object is not live",
            ));
        }
        if !matches!(
            self.contract()
                .members("")
                .and_then(|members| members.get(name)),
            Some(tembra_contracts::Member::Opaque { .. })
        ) {
            return Err(RuntimeError::new(
                ErrorCode::NotInVariant,
                "opaque",
                "member is not exposed by the consumer contract",
            ));
        }
        let implementation = self.implementation_view()?;
        let member = implementation.opaque.get(name).ok_or_else(|| {
            RuntimeError::new(ErrorCode::NotInVariant, "opaque", "opaque member missing")
        })?;
        member()?
            .downcast::<T>()
            .map_err(|_| RuntimeError::invalid("native_type", "opaque member type mismatch"))
    }

    /// Waits for the consumer shape and, for a full state claim, every required snapshot.
    /// Method-only claims can become ready before state hydration.
    ///
    /// # Errors
    /// Returns activation, hydration, release, revocation, adapter or view-validation failures.
    /// A timeout releases this claim only. Dropping the wait unregisters its readiness waiter, but
    /// retaining this reference continues to retain its claim. Any pending reload binding
    /// waits for coherent publication, including a replaced object or a fresh object of an
    /// unchanged service. Awaiting that publication from a resource opener or initializer
    /// in the same preparation creates a cyclic dependency.
    pub async fn ready(&self) -> Result<()> {
        self.claim.check()?;
        let host = self
            .claim
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        if host.in_factory.get() > 0 {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "factory",
                "reference readiness cannot be awaited inside a factory",
            ));
        }
        self.claim.notify();
        self.claim.check()?;
        if self.status() == RefStatus::Live {
            return Ok(());
        }
        let (send, receive) = oneshot::channel();
        let id = self.claim.next_waiter.get() + 1;
        self.claim.next_waiter.set(id);
        self.claim.waiters.borrow_mut().insert(id, send);
        let _waiter = ClaimWaiter {
            claim: Rc::downgrade(&self.claim),
            id,
        };
        let waiting = async {
            receive.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "activation",
                    "readiness interrupted",
                ))
            })
        }
        .boxed_local();
        let result =
            match futures::future::select(waiting, host.driver.sleep(host.ready_timeout)).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => {
                    self.release();
                    Err(RuntimeError::unavailable(
                        "ready_timeout",
                        "reference readiness deadline elapsed",
                    ))
                }
            };
        result?;
        self.claim.check()?;
        if self.claim.local_object().is_some() {
            self.implementation_view().map(|_| ())
        } else {
            Ok(())
        }
    }

    /// Invokes a native local method after checking Rust argument/result type identities.
    /// Values remain native; this channel does not apply portable JSON schemas.
    ///
    /// # Errors
    /// Refuses ended claims, invalid contexts, factory-time access, absent methods and type
    /// mismatches before dispatch. Handler errors may follow external effects; no rollback
    /// is provided. Cancellation is cooperative after dispatch, and dropping the future drops
    /// the handler future and expires its Context. Independently retained InvocationLeases keep
    /// physical resource ownership until their jobs settle; external work is not undone.
    /// A readiness timeout releases this reference's entire claim, including its clones;
    /// independently acquired claims remain live. A pending reload binding refuses method
    /// dispatch with `not_ready/candidate_private` before waiting for publication. Existing
    /// dependency claims still select the actual entered Context's installed implementation.
    pub async fn call<A: 'static, R: 'static>(
        &self,
        name: &str,
        args: A,
        ctx: Option<&Context>,
    ) -> Result<R> {
        self.call_with_receipt(name, args, ctx, None).await
    }

    /// Starts an opt-in tracked native call when polled.
    ///
    /// # Errors
    /// Has the same checked admission and effect semantics as `call`. The receipt observes
    /// physical ownership independently from the returned result or caller cancellation.
    pub fn call_tracked<A: 'static, R: 'static>(
        &self,
        name: &str,
        args: A,
        ctx: Option<&Context>,
    ) -> crate::ManagedCall<R> {
        let receipt = crate::InvocationReceipt::new();
        let observed = receipt.clone();
        let reference = self.clone();
        let name = name.to_owned();
        let ctx = ctx.cloned();
        crate::ManagedCall::new(
            receipt,
            async move {
                reference
                    .call_with_receipt(&name, args, ctx.as_ref(), Some(&observed))
                    .await
            }
            .boxed_local(),
        )
    }

    pub(crate) async fn call_with_receipt<A: 'static, R: 'static>(
        &self,
        name: &str,
        args: A,
        ctx: Option<&Context>,
        receipt: Option<&crate::InvocationReceipt>,
    ) -> Result<R> {
        self.claim.local()?;
        let host = self
            .claim
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        if host.in_factory.get() > 0 {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "factory",
                "references cannot be called inside a factory",
            ));
        }
        self.claim.check()?;
        self.claim.check_method_publication(&host)?;
        let (context, _root_scope) = if let Some(ctx) = ctx {
            ctx.check(host.id)?;
            (ctx.clone(), None)
        } else {
            let (ctx, scope) = host.new_root(host.principal.clone());
            (ctx, Some(scope))
        };
        let _activation_wait = self
            .claim
            .local_object()
            .map(|object| host.initializer_wait(&context, object))
            .transpose()?
            .flatten();
        let cancelled =
            futures::future::select(context.cancellation().cancelled(), context.scope_ended())
                .map(|either| match either {
                    futures::future::Either::Left((error, _))
                    | futures::future::Either::Right((error, _)) => error,
                })
                .boxed_local();
        let ready = match futures::future::select(self.ready().boxed_local(), cancelled).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right((error, _)) => return Err(error),
        };
        context.check(host.id)?;
        ready?;
        self.claim.check()?;
        if self.claim.local()?.phase.get() != Phase::Live {
            return Err(RuntimeError::new(
                ErrorCode::Gone,
                "gone",
                "object is not live",
            ));
        }
        let object = self.claim.local()?.clone();
        let implementation = self.implementation_view_at(context.code_version())?;
        if !matches!(
            self.contract()
                .members(implementation.variant.as_deref().unwrap_or(""))
                .and_then(|members| members.get(name)),
            Some(tembra_contracts::Member::Method { .. })
        ) {
            return Err(RuntimeError::new(
                ErrorCode::NotInVariant,
                "method",
                "method is absent from the consumer contract",
            ));
        }
        let method = implementation.methods.get(name).ok_or_else(|| {
            RuntimeError::new(
                ErrorCode::NotInVariant,
                "method",
                "method is not in the reference shape",
            )
        })?;
        let returns_value = matches!(
            self.claim
                .contract
                .members("")
                .and_then(|members| members.get(name)),
            Some(tembra_contracts::Member::Method {
                result: Some(_),
                ..
            })
        );
        if method.args != TypeId::of::<A>()
            || (returns_value && method.result != TypeId::of::<R>())
            || (!returns_value && TypeId::of::<R>() != TypeId::of::<()>())
        {
            return Err(RuntimeError::invalid(
                "native_type",
                "native method argument or result type mismatch",
            ));
        }
        let invoke = method.invoke.clone();
        let (child, _invocation) = crate::invocation::enter_with_receipt(
            &host,
            object.clone(),
            implementation.clone(),
            &context,
            receipt,
        )?;
        let result = invoke(Box::new(args), child).await?;
        if !returns_value {
            return (Box::new(()) as Box<dyn std::any::Any>)
                .downcast::<R>()
                .map(|value| *value)
                .map_err(|_| {
                    RuntimeError::invalid("native_type", "void method requires unit result")
                });
        }
        result.downcast::<R>().map(|value| *value).map_err(|_| {
            RuntimeError::invalid("native_type", "native method returned an unexpected type")
        })
    }

    fn implementation_view(&self) -> Result<Rc<crate::Implementation>> {
        self.implementation_view_at(self.claim.binding_version()?)
    }

    fn implementation_view_at(&self, version: u64) -> Result<Rc<crate::Implementation>> {
        let object = self.claim.local()?;
        if let Some(view) = self.claim.host.upgrade().and_then(|host| {
            host.ready_binding_view(object.id, version, self.contract().fingerprint())
        }) {
            return Ok(view);
        }
        let installed = object
            .installed
            .borrow()
            .get(&version)
            .cloned()
            .ok_or_else(crate::epoch::superseded)?;
        let current = installed.implementation.clone();
        if installed.service.contract().version() == self.contract().version() {
            HostInner::validate_view(self.contract(), &current, false)?;
            return Ok(current);
        }
        if let Some(view) = installed
            .adapters
            .borrow()
            .get(self.contract().fingerprint())
            .cloned()
        {
            return Ok(view);
        }
        let adapter = installed
            .service
            .options()
            .accepts
            .get(&self.contract().version())
            .cloned()
            .ok_or_else(|| {
                RuntimeError::new(
                    ErrorCode::ContractMismatch,
                    "adapter",
                    "consumer adapter is unavailable",
                )
            })?;
        let view = Rc::new(adapter(current)?);
        HostInner::validate_view(self.contract(), &view, false)?;
        installed
            .adapters
            .borrow_mut()
            .insert(self.contract().fingerprint().into(), view.clone());
        Ok(view)
    }

    /// Invokes a schema-validated portable method. `None` means absence, not JSON null.
    ///
    /// Arguments are cleaned without inserting defaults, then validated before the handler
    /// runs. Declared results are checked after the handler completes; consumer void views
    /// discard a valid provider result. Native capabilities require [`Self::call`].
    ///
    /// # Errors
    ///
    /// Returns lifecycle/context errors before dispatch, `not_in_variant` for unavailable
    /// methods, and `invalid` for argument presence, schema, or binding errors. These
    /// refusals do not invoke the handler. Handler errors, serialization failures and invalid
    /// results can occur after effects have happened; no rollback is provided.
    ///
    /// Cancellation while waiting for readiness ends that wait. After dispatch it is
    /// cooperative: the handler receives a child context and owns its response to
    /// cancellation. Dropping a local call drops its handler future and expires its Context.
    /// Independently retained InvocationLeases keep resources until their physical jobs settle.
    /// Dropping a remote wait sends Cancel; the provider retains its job and resource pins
    /// until physical settlement. A lost admitted remote result has unknown outcome and
    /// is never replayed automatically. No path undoes external work already started.
    /// Method completion is not a state-observation fence.
    /// A readiness timeout releases this reference's entire claim, including its clones;
    /// independently acquired claims remain live.
    /// Pending reload bindings refuse `not_ready/candidate_private` before readiness.
    /// Existing dependency claims still dispatch at the actual entered Context's epoch.
    pub async fn call_json(
        &self,
        name: &str,
        args: Option<serde_json::Value>,
        ctx: Option<&Context>,
    ) -> Result<Option<serde_json::Value>> {
        self.call_json_with_receipt(name, args, ctx, None).await
    }
    pub(crate) async fn call_json_with_receipt(
        &self,
        name: &str,
        args: Option<serde_json::Value>,
        ctx: Option<&Context>,
        receipt: Option<&crate::InvocationReceipt>,
    ) -> Result<Option<serde_json::Value>> {
        use tembra_contracts::{Member, ValueSchema};
        let host = self
            .claim
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        if host.in_factory.get() > 0 {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "factory",
                "references cannot be called inside a factory",
            ));
        }
        self.claim.check()?;
        self.claim.check_method_publication(&host)?;
        let (context, _root_scope) = if let Some(ctx) = ctx {
            ctx.check(host.id)?;
            (ctx.clone(), None)
        } else {
            let (ctx, guard) = host.new_root(host.principal.clone());
            (ctx, Some(guard))
        };
        let _activation_wait = self
            .claim
            .local_object()
            .map(|object| host.initializer_wait(&context, object))
            .transpose()?
            .flatten();
        let stopped =
            futures::future::select(context.cancellation().cancelled(), context.scope_ended())
                .map(|value| match value {
                    futures::future::Either::Left((error, _))
                    | futures::future::Either::Right((error, _)) => error,
                })
                .boxed_local();
        let ready = match futures::future::select(self.ready().boxed_local(), stopped).await {
            futures::future::Either::Left((ready, _)) => ready,
            futures::future::Either::Right((error, _)) => return Err(error),
        };
        context.check(host.id)?;
        ready?;
        self.claim.check()?;
        if let ReferenceBacking::Remote(remote) = &self.claim.backing {
            return remote.call(name, args, &context).await;
        }
        let object = self.claim.local()?.clone();
        let implementation = self.implementation_view_at(context.code_version())?;
        let variant = implementation.variant.as_deref().unwrap_or("");
        let consumer = self
            .contract()
            .members(variant)
            .and_then(|members| members.get(name));
        let Some(Member::Method {
            args: consumer_args,
            result: consumer_result,
        }) = consumer
        else {
            return Err(RuntimeError::new(
                ErrorCode::NotInVariant,
                "method",
                "method is absent from the consumer contract",
            ));
        };
        let provider = object
            .installed
            .borrow()
            .get(&context.code_version())
            .map(|installed| installed.service.contract().clone())
            .ok_or_else(crate::epoch::superseded)?;
        let validation_contract = if provider.version() == self.contract().version() {
            &provider
        } else {
            self.contract()
        };
        let Some(Member::Method {
            args: provider_args,
            result: provider_result,
        }) = validation_contract
            .members(variant)
            .and_then(|members| members.get(name))
        else {
            return Err(RuntimeError::new(
                ErrorCode::ContractMismatch,
                "method",
                "selected implementation has no method schema",
            ));
        };
        if [
            consumer_args,
            consumer_result,
            provider_args,
            provider_result,
        ]
        .into_iter()
        .flatten()
        .any(|schema| matches!(schema, ValueSchema::Native(_)))
        {
            return Err(RuntimeError::invalid(
                "native_type",
                "native method schemas require an explicit portable binding contract",
            ));
        }
        let args = match (consumer_args, provider_args, args) {
            (None, None, None) => None,
            (Some(_), Some(_), Some(value)) => {
                let value = self
                    .contract()
                    .clean_method_args(variant, name, &value)
                    .map_err(|error| RuntimeError::invalid("args", error.message))?;
                self.contract()
                    .validate_method_args(variant, name, &value)
                    .map_err(|error| RuntimeError::invalid("args", error.message))?;
                let value = validation_contract
                    .clean_method_args(variant, name, &value)
                    .map_err(|error| RuntimeError::invalid("args", error.message))?;
                validation_contract
                    .validate_method_args(variant, name, &value)
                    .map_err(|error| RuntimeError::invalid("args", error.message))?;
                Some(value)
            }
            _ => {
                return Err(RuntimeError::invalid(
                    "args",
                    "argument presence differs from the bound method contract",
                ));
            }
        };
        let invoke = implementation
            .json_methods
            .get(name)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::invalid(
                    "native_type",
                    "method has no portable binding; use the native typed route",
                )
            })?;
        let (child, _invocation) = crate::invocation::enter_with_receipt(
            &host,
            object.clone(),
            implementation.clone(),
            &context,
            receipt,
        )?;
        let result = invoke(args, child).await?;
        if let Some(schema) = provider_result {
            let ValueSchema::Portable(_) = schema else {
                return Err(RuntimeError::invalid(
                    "native_type",
                    "native result cannot cross a portable boundary",
                ));
            };
            let value = result.as_ref().ok_or_else(|| {
                RuntimeError::invalid("result", "provider omitted its declared result")
            })?;
            validation_contract
                .validate_method_result(variant, name, value)
                .map_err(|error| RuntimeError::invalid("result", error.message))?;
        }
        if consumer_result.is_none() {
            return Ok(None);
        }
        let value = result.ok_or_else(|| {
            RuntimeError::invalid("result", "provider omitted the consumer result")
        })?;
        if let Some(ValueSchema::Portable(_)) = consumer_result {
            self.contract()
                .validate_method_result(variant, name, &value)
                .map_err(|error| RuntimeError::invalid("result", error.message))?;
        }
        Ok(Some(value))
    }
}

struct ClaimWaiter {
    claim: Weak<Claim>,
    id: u64,
}
impl Drop for ClaimWaiter {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.upgrade() {
            claim.waiters.borrow_mut().remove(&self.id);
        }
    }
}
