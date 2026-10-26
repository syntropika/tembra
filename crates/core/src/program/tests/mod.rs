use super::*;
use crate::{Action, Address, Facet, Service};
use std::collections::VecDeque;

fn host_scope(host: &Host, resources: Vec<Id>, parents: Vec<Id>) -> Id {
    let id = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id,
        owner: Owner::Host,
        facet: 0,
        parents,
        dependencies: vec![],
        resources,
        state_version: None,
        migrate: false,
        active: true,
        closing: false,
    })
    .unwrap();
    id
}

fn complete(host: &Host, program: Id, effects: &[Effect]) {
    host.complete_step(
        program,
        effects
            .iter()
            .map(|_| Completion {
                value: None,
                error: None,
            })
            .collect(),
    )
    .unwrap();
}

fn waiting_resource(host: &Host, name: &str) -> (Id, Id, Id) {
    let base = host
        .resource(Owner::Host, format!("base-{name}"), 1, false, vec![])
        .unwrap();
    let dependent = host
        .resource(
            Owner::Host,
            format!("dependent-{name}"),
            1,
            false,
            vec![base.id],
        )
        .unwrap();
    let scope = host_scope(host, vec![dependent.id], vec![]);
    host.inner
        .borrow_mut()
        .scopes
        .get_mut(&scope)
        .unwrap()
        .active = false;
    let program = host.activate_scope(scope).unwrap();
    host.run_program(program).unwrap();
    (base.id, dependent.id, program)
}

fn settle_failed_consumer(host: &Host, consumer: Id) {
    let mut failed = false;
    for _ in 0..32 {
        for action in host.drain() {
            match action {
                Action::Execute { program, effects } => {
                    for effect in &effects {
                        assert!(!matches!(effect, Effect::Open { .. }));
                        if let Effect::Close { resource } = effect {
                            host.resource_closed(*resource).unwrap();
                        }
                    }
                    complete(host, program, &effects);
                }
                Action::ProgramComplete { program, error } if program == consumer => {
                    assert_eq!(error.unwrap().reason, Reason::Resource.as_str());
                    failed = true;
                }
                Action::ProgramComplete { error, .. } => assert!(error.is_none()),
                _ => {}
            }
        }
    }
    assert!(failed);
    assert!(host.inner.borrow().programs.is_empty());
}

mod cleanup;
mod migration;
mod readiness;
mod recovery;
mod scheduler;
