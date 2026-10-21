//! Prospective declarations and private object discovery; no services are installed here.
use super::*;
use crate::host::{Address, HostInner, Service, contract_error};
use crate::object::{ObjectRecord, Phase};
use crate::{GetOptions, Lifetime, Reference, Result, RuntimeError};
use futures::{
    FutureExt,
    future::{LocalBoxFuture, Shared},
};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};
use tembra_contracts::{Contract, ContractKind};

type Opening = Shared<LocalBoxFuture<'static, Result<()>>>;
pub(crate) struct CandidateCatalog {
    host: Weak<HostInner>,
    services: HashMap<String, Service>,
    changed: HashSet<String>,
    pub(crate) pending: RefCell<HashMap<Address, Rc<ObjectRecord>>>,
    plans: RefCell<HashMap<u64, Weak<CandidateDeclarations>>>,
    openings: RefCell<HashMap<u64, Opening>>,
    implementations: RefCell<HashMap<u64, Weak<crate::Implementation>>>,
    views: RefCell<HashMap<u64, HashMap<String, Rc<crate::Implementation>>>>,
    signal: CancellationToken,
    version: crate::epoch::CodeEpoch,
}
impl CandidateCatalog {
    pub(super) fn new(
        host: &Rc<HostInner>,
        batch: &crate::host::facets::PreparedFacetBatch,
        signal: CancellationToken,
        version: crate::epoch::CodeEpoch,
    ) -> Rc<Self> {
        let mut services = host.services.borrow().clone();
        for removed in host.reload_removed_services(batch) {
            services.remove(&removed);
        }
        services.extend(batch.services.clone());
        Rc::new(Self {
            host: Rc::downgrade(host),
            services,
            changed: batch.services.keys().cloned().collect(),
            pending: RefCell::default(),
            plans: RefCell::default(),
            openings: RefCell::default(),
            implementations: RefCell::default(),
            views: RefCell::default(),
            signal,
            version,
        })
    }
    pub(crate) fn register(&self, object: &ObjectRecord, candidate: &Rc<CandidateDeclarations>) {
        self.plans
            .borrow_mut()
            .insert(object.id, Rc::downgrade(candidate));
    }
    pub(super) fn recorded(&self, object: u64, implementation: &Rc<crate::Implementation>) {
        self.implementations
            .borrow_mut()
            .insert(object, Rc::downgrade(implementation));
    }
    pub(super) fn adapter_views(&self, object: u64) -> HashMap<String, Rc<crate::Implementation>> {
        self.views
            .borrow()
            .get(&object)
            .cloned()
            .unwrap_or_default()
    }
    pub(super) fn state_links(
        &self,
        references: Vec<Reference>,
    ) -> Result<Vec<(crate::Readable, crate::Readable)>> {
        let mut links = Vec::new();
        for reference in references {
            if reference.claim.binding_ended() {
                continue;
            }
            let Some(object) = reference.claim.local_object() else {
                continue;
            };
            let Some(candidate) = self.plans.borrow().get(&object.id).and_then(Weak::upgrade)
            else {
                continue;
            };
            let current = self
                .implementations
                .borrow()
                .get(&object.id)
                .and_then(Weak::upgrade)
                .ok_or_else(|| {
                    RuntimeError::invalid("reload_state", "candidate was not recorded")
                })?;
            candidate.service.bind(reference.contract())?;
            let cached = self
                .views
                .borrow()
                .get(&object.id)
                .and_then(|views| views.get(reference.contract().fingerprint()))
                .cloned();
            let view = if reference.contract().version() == candidate.service.contract().version() {
                HostInner::validate_view(reference.contract(), &current, false)?;
                current
            } else if let Some(view) = cached {
                view
            } else {
                let adapter = candidate
                    .service
                    .options()
                    .accepts
                    .get(&reference.contract().version())
                    .expect("checked candidate adapter")
                    .clone();
                let mut view = adapter(current)?;
                HostInner::validate_view(reference.contract(), &view, false)?;
                let members = reference
                    .contract()
                    .members(view.variant.as_deref().unwrap_or(""))
                    .expect("validated adapter variant");
                for (name, member) in members {
                    let tembra_contracts::Member::State { schema } = member else {
                        continue;
                    };
                    let source = view.states.get_mut(name).expect("validated consumer state");
                    if !object.state.owns(source) {
                        *source = candidate.graph.forward(source.clone())?;
                    }
                    candidate.graph.expose(source, schema.clone())?;
                }
                let view = Rc::new(view);
                self.views
                    .borrow_mut()
                    .entry(object.id)
                    .or_default()
                    .insert(reference.contract().fingerprint().into(), view.clone());
                view
            };
            links.extend(reference.claim.projection_links(&view));
        }
        Ok(links)
    }
    pub(crate) fn hold(
        self: &Rc<Self>,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        self.signal.check()?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let input = contract.normalize_input(&input).map_err(contract_error)?;
        if contract.kind() == ContractKind::Remote
            && contract
                .resolve_host(&input)
                .map_err(contract_error)?
                .as_deref()
                != Some(host.name.as_str())
        {
            return host.hold(contract, input, options);
        }
        let service = self.services.get(contract.name()).cloned().ok_or_else(|| {
            RuntimeError::unavailable("unknown_type", "candidate type is not served")
        })?;
        service.bind(&contract)?;
        if contract
            .resolve_host(&input)
            .map_err(contract_error)?
            .is_some_and(|name| name != host.name)
        {
            return Err(RuntimeError::invalid(
                "placement",
                "candidate input resolves to another Host",
            ));
        }
        let provider_input = service
            .contract()
            .normalize_input(&input)
            .map_err(contract_error)?;
        let identity = contract.identity(&input).map_err(contract_error)?;
        if identity
            != service
                .contract()
                .identity(&provider_input)
                .map_err(contract_error)?
        {
            return Err(RuntimeError::new(
                crate::ErrorCode::ContractMismatch,
                "identity",
                "candidate input identity differs",
            ));
        }
        let principal = if service.options().lifetime == Lifetime::Host {
            if identity
                != contract
                    .identity(&serde_json::json!({}))
                    .map_err(contract_error)?
            {
                return Err(RuntimeError::invalid(
                    "input",
                    "host object input must identify its singleton",
                ));
            }
            host.principal.clone()
        } else {
            options.principal.unwrap_or_else(|| host.principal.clone())
        };
        let key = Address(
            contract.name().into(),
            identity,
            (host.principal_key)(&principal),
        );
        let installed = host
            .objects
            .borrow()
            .get(&key)
            .filter(|object| {
                matches!(object.phase.get(), Phase::Activating | Phase::Live)
                    && !object.cancellation.is_cancelled()
            })
            .cloned();
        let unchanged = installed.is_some() && !self.changed.contains(contract.name());
        let pending = self.pending.borrow().get(&key).cloned();
        let object = if let Some(object) = installed.or(pending) {
            object
        } else {
            let object = host.new_object(key.clone(), provider_input, principal, service);
            self.pending.borrow_mut().insert(key, object.clone());
            object
        };
        if unchanged {
            return Ok(Reference::new(&host, object, contract, options.state));
        }
        Ok(Reference::pending(
            &host,
            object,
            contract,
            options.state,
            self.version.get(),
        ))
    }
    pub(crate) fn ready(
        self: &Rc<Self>,
        reference: Reference,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let Some(object) = reference.claim.local_object().cloned() else {
            return async move { reference.ready().await }.boxed_local();
        };
        let candidate = self.plans.borrow().get(&object.id).and_then(Weak::upgrade);
        let Some(candidate) = candidate else {
            return async move { reference.ready().await }.boxed_local();
        };
        if let Some(opening) = self.openings.borrow().get(&object.id).cloned() {
            return opening.boxed_local();
        }
        let signal = self.signal.clone();
        let resources = candidate.resources.clone();
        let opening = async move { resources.open_nonexclusive(signal).await }
            .boxed_local()
            .shared();
        self.openings
            .borrow_mut()
            .insert(object.id, opening.clone());
        opening.boxed_local()
    }
    pub(crate) fn reaches(
        &self,
        object: &ObjectRecord,
        target: u64,
        visited: &mut std::collections::HashSet<u64>,
    ) -> bool {
        if object.id == target {
            return true;
        }
        if !visited.insert(object.id) {
            return false;
        }
        let candidate = self.plans.borrow().get(&object.id).and_then(Weak::upgrade);
        let dependencies = candidate
            .map(|candidate| {
                candidate
                    .dependencies
                    .borrow()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| object.dependencies.borrow().values().cloned().collect());
        dependencies
            .iter()
            .filter_map(|reference| reference.claim.local_object())
            .any(|dependency| self.reaches(dependency, target, visited))
    }
}
