//! Immutable binding inputs and successful compatibility proofs owned by a service definition.
use super::{check_binding, facets};
use crate::{Factory, Result, ServeOptions};
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::{Rc, Weak},
};
use tembra_contracts::Contract;

const MAX_CONSUMER_PROOFS: usize = 16;

// Only this module can construct or replace the inputs certified by these proofs.
// Every replacement starts with fresh proofs; Service clones share the same proof owner.
pub(super) struct ServiceDefinition {
    contract: Rc<Contract>,
    options: ServeOptions,
    proofs: Rc<BindingProofs>,
}

impl ServiceDefinition {
    pub(super) fn checked(contract: &Rc<Contract>, options: &ServeOptions) -> Result<Self> {
        check_binding(contract, contract, options)?;
        Ok(Self {
            contract: contract.clone(),
            options: options.clone(),
            proofs: Rc::new(BindingProofs::default()),
        })
    }
}

#[derive(Default)]
struct BindingProofs {
    consumers: RefCell<VecDeque<Weak<Contract>>>,
}

impl BindingProofs {
    fn bind_with(
        &self,
        provider: &Rc<Contract>,
        consumer: &Rc<Contract>,
        prove: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // Only validated Service values call this with their immutable provider inputs.
        if Rc::ptr_eq(provider, consumer) {
            return Ok(());
        }
        {
            let mut consumers = self.consumers.borrow_mut();
            consumers.retain(|entry| entry.strong_count() != 0);
            if consumers.iter().any(|entry| {
                entry
                    .upgrade()
                    .is_some_and(|contract| Rc::ptr_eq(&contract, consumer))
            }) {
                return Ok(());
            }
        }
        // No memo borrow spans validation, and errors never enter the positive memo.
        prove()?;
        let mut consumers = self.consumers.borrow_mut();
        if consumers.len() == MAX_CONSUMER_PROOFS {
            consumers.pop_front();
        }
        consumers.push_back(Rc::downgrade(consumer));
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Service {
    // Keep the original contract, factory, options destruction order: captures may reenter.
    contract: Rc<Contract>,
    pub(crate) factory: Factory,
    options: ServeOptions,
    pub(crate) ordinal: facets::InstalledServiceId,
    pub(crate) owner: Option<facets::FacetRunId>,
    pub(crate) owner_name: Option<Rc<str>>,
    proofs: Rc<BindingProofs>,
}

impl Service {
    pub(super) fn new(
        definition: ServiceDefinition,
        factory: Factory,
        ordinal: facets::InstalledServiceId,
        owner: Option<facets::FacetRunId>,
        owner_name: Option<Rc<str>>,
    ) -> Self {
        let ServiceDefinition {
            contract,
            options,
            proofs,
        } = definition;
        Self {
            contract,
            factory,
            options,
            ordinal,
            owner,
            owner_name,
            proofs,
        }
    }

    pub(crate) fn contract(&self) -> &Rc<Contract> {
        &self.contract
    }

    pub(crate) fn options(&self) -> &ServeOptions {
        &self.options
    }

    pub(crate) fn bind(&self, consumer: &Rc<Contract>) -> Result<()> {
        self.proofs.bind_with(self.contract(), consumer, || {
            check_binding(self.contract(), consumer, self.options())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, collections::BTreeMap};
    use tembra_contracts::{Member, ValueSchema};

    fn contract(version: u64) -> Rc<Contract> {
        Rc::new(Contract::local("proof", version, json!({}), BTreeMap::new()).unwrap())
    }

    fn service(contract: &Rc<Contract>, options: &ServeOptions) -> Service {
        Service::new(
            ServiceDefinition::checked(contract, options).unwrap(),
            Rc::new(|_, _, _| Ok(crate::Implementation::new())),
            facets::InstalledServiceId(1),
            None,
            None,
        )
    }

    #[test]
    fn self_proof_must_succeed_before_a_definition_exists() {
        let contract = Rc::new(Contract::local("proof", 1, json!({}), BTreeMap::from([(
            "call".into(),
            Member::Method {
                args: Some(ValueSchema::Portable(json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}},
                    {"type": "object", "properties": {"b": {"type": "integer"}}, "additionalProperties": true}
                ]}))),
                result: None,
            },
        )])).unwrap());
        let error = ServiceDefinition::checked(&contract, &ServeOptions::default())
            .err()
            .unwrap();
        assert_eq!(error.reason, "ambiguous_normalization");
    }

    #[test]
    fn validated_self_and_cloned_services_reuse_the_same_proof() {
        let provider = contract(1);
        let service = service(&provider, &ServeOptions::default());
        let alias = service.clone();
        alias
            .proofs
            .bind_with(alias.contract(), &provider, || {
                panic!("self proof was repeated")
            })
            .unwrap();
        let consumer = contract(1);
        service.bind(&consumer).unwrap();
        alias
            .proofs
            .bind_with(alias.contract(), &consumer, || {
                panic!("clone lost a positive proof")
            })
            .unwrap();
        assert!(Rc::ptr_eq(&service.proofs, &alias.proofs));
    }

    #[test]
    fn distinct_equal_contracts_each_require_their_first_full_proof() {
        let provider = contract(1);
        let service = service(&provider, &ServeOptions::default());
        let first = contract(1);
        let second = contract(1);
        assert_eq!(first.fingerprint(), second.fingerprint());
        let proofs = Cell::new(0);
        for consumer in [&first, &first, &second, &second] {
            service
                .proofs
                .bind_with(service.contract(), consumer, || {
                    proofs.set(proofs.get() + 1);
                    check_binding(&provider, consumer, service.options())
                })
                .unwrap();
        }
        assert_eq!(proofs.get(), 2);
    }

    #[test]
    fn failed_proofs_are_never_cached() {
        let service = service(&contract(1), &ServeOptions::default());
        let consumer = contract(2);
        let proofs = Cell::new(0);
        for _ in 0..2 {
            let error = service
                .proofs
                .bind_with(service.contract(), &consumer, || {
                    proofs.set(proofs.get() + 1);
                    check_binding(service.contract(), &consumer, service.options())
                })
                .unwrap_err();
            assert_eq!(error.reason, "version");
        }
        assert_eq!(proofs.get(), 2);
        assert!(service.proofs.consumers.borrow().is_empty());
    }

    #[test]
    fn weak_memo_does_not_retain_consumers_and_cleans_expired_entries() {
        let service = service(&contract(1), &ServeOptions::default());
        let consumer = contract(1);
        let weak = Rc::downgrade(&consumer);
        service.bind(&consumer).unwrap();
        assert_eq!(Rc::strong_count(&consumer), 1);
        drop(consumer);
        assert!(weak.upgrade().is_none());
        let next = contract(1);
        service.bind(&next).unwrap();
        assert_eq!(service.proofs.consumers.borrow().len(), 1);
        assert!(
            service
                .proofs
                .consumers
                .borrow()
                .iter()
                .all(|entry| entry.upgrade().is_some())
        );
    }

    #[test]
    fn live_consumers_are_bounded_and_eviction_requires_a_fresh_proof() {
        let service = service(&contract(1), &ServeOptions::default());
        let consumers = (0..=MAX_CONSUMER_PROOFS)
            .map(|_| contract(1))
            .collect::<Vec<_>>();
        for consumer in &consumers {
            service.bind(consumer).unwrap();
        }
        assert_eq!(service.proofs.consumers.borrow().len(), MAX_CONSUMER_PROOFS);
        let proofs = Cell::new(0);
        service
            .proofs
            .bind_with(service.contract(), &consumers[0], || {
                proofs.set(proofs.get() + 1);
                check_binding(service.contract(), &consumers[0], service.options())
            })
            .unwrap();
        assert_eq!(proofs.get(), 1);
        assert_eq!(service.proofs.consumers.borrow().len(), MAX_CONSUMER_PROOFS);
    }

    #[test]
    fn replacement_definition_never_inherits_old_options_or_proofs() {
        let provider = contract(1);
        let consumer = contract(2);
        let options = ServeOptions::default()
            .with_adapter(2, Rc::new(|implementation| Ok((*implementation).clone())));
        let old = service(&provider, &options);
        old.bind(&consumer).unwrap();
        let replacement = service(&provider, &ServeOptions::default());
        assert!(!Rc::ptr_eq(&old.proofs, &replacement.proofs));
        assert_eq!(replacement.bind(&consumer).unwrap_err().reason, "version");
        old.bind(&consumer).unwrap();
    }
}
