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

#[test]
fn overlapping_cleanup_has_one_physical_closer_and_waits_for_it() {
    let host = Host::new();
    let resource = host
        .resource(Owner::Host, "shared".into(), 1, false, vec![])
        .unwrap();
    host.resource_opened(resource.id, host.allocate_handle().unwrap())
        .unwrap();
    let a = host_scope(&host, vec![resource.id], vec![]);
    let b = host_scope(&host, vec![resource.id], vec![]);
    for scope in [a, b] {
        let program = host.cleanup_scopes(vec![scope], None).unwrap();
        host.run_program(program).unwrap();
    }
    for action in host.drain() {
        if let Action::Execute { program, effects } = action {
            complete(&host, program, &effects);
        }
    }
    let pending = host.drain();
    let close: Vec<_> = pending
        .into_iter()
        .filter_map(|action| match action {
            Action::Execute { program, effects } => Some((program, effects)),
            _ => None,
        })
        .collect();
    assert_eq!(close.len(), 1);
    assert_eq!(
        close[0].1,
        vec![Effect::Close {
            resource: resource.id
        }]
    );
    let visits = host.inner.borrow().program_visits;
    for _ in 0..100 {
        assert!(host.drain().is_empty());
    }
    assert_eq!(host.inner.borrow().program_visits, visits);
    assert!(host.scope_exists(a) && host.scope_exists(b));
    assert!(!host.close_resource(resource.id).unwrap());
    host.resource_closed(resource.id).unwrap();
    complete(&host, close[0].0, &close[0].1);
    let mut dropped = BTreeSet::new();
    let mut pending = VecDeque::from(host.drain());
    while let Some(action) = pending.pop_front() {
        if let Action::Execute { program, effects } = action {
            for effect in &effects {
                let Effect::Drop { scope } = effect else {
                    panic!("duplicate close: {effect:?}")
                };
                assert!(dropped.insert(*scope));
            }
            complete(&host, program, &effects);
        }
        pending.extend(host.drain());
    }
    assert_eq!(dropped, BTreeSet::from([a, b]));
    assert_eq!(host.diagnostics().resources, 0);
    assert!(host.inner.borrow().programs.is_empty());
}

#[test]
fn child_completion_wakes_only_its_waiting_parent() {
    let host = Host::new();
    let parent = host_scope(&host, vec![], vec![]);
    let child = host_scope(&host, vec![], vec![parent]);
    for scope in [parent, child] {
        let program = host.cleanup_scopes(vec![scope], None).unwrap();
        host.run_program(program).unwrap();
    }
    let mut pending = VecDeque::from(host.drain());
    let mut dropped = vec![];
    while let Some(action) = pending.pop_front() {
        if let Action::Execute { program, effects } = action {
            for effect in &effects {
                if let Effect::Drop { scope } = effect {
                    dropped.push(*scope);
                }
            }
            complete(&host, program, &effects);
        }
        pending.extend(host.drain());
    }
    assert_eq!(dropped, [child, parent]);
    assert!(host.inner.borrow().programs.is_empty());
}

#[test]
fn restored_dependencies_and_memberships_use_the_new_identity() {
    let host = Host::new();
    let base = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    let dependent = host
        .resource(Owner::Host, "dependent".into(), 1, false, vec![base.id])
        .unwrap();
    let scope = host_scope(&host, vec![base.id, dependent.id], vec![]);
    assert!(host.close_resource(dependent.id).unwrap());
    host.resource_closed(dependent.id).unwrap();
    assert!(host.close_resource(base.id).unwrap());
    host.resource_closed(base.id).unwrap();
    let next = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    host.replace_resource(base.id, next.id);
    let restored = host
        .resource(Owner::Host, "dependent".into(), 1, false, vec![next.id])
        .unwrap();
    host.replace_resource(dependent.id, restored.id);
    assert_eq!(
        host.inner.borrow().scopes[&scope].resources,
        [next.id, restored.id]
    );
    assert!(!host.close_resource(next.id).unwrap());
    assert!(host.close_resource(restored.id).unwrap());
    host.resource_closed(restored.id).unwrap();
    assert!(host.close_resource(next.id).unwrap());
}

#[test]
fn successor_activity_does_not_block_old_resource_cleanup() {
    let host = Host::new();
    let old = host
        .prepare(vec![Facet::new("example", 1)], vec![])
        .unwrap();
    host.commit(old.id, vec![]).unwrap();
    let owner = Owner::Facet(old.facets[0].owner);
    let resource = host
        .resource(owner, "connection".into(), 1, false, vec![])
        .unwrap();
    host.resource_opened(resource.id, host.allocate_handle().unwrap())
        .unwrap();
    let register = |facet, resources| {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner,
            facet,
            parents: vec![],
            dependencies: vec![],
            resources,
            state_version: None,
            migrate: false,
            active: true,
            closing: false,
        })
        .unwrap();
        id
    };
    let previous = register(old.facets[0].id, vec![resource.id]);
    let next = host
        .prepare(vec![Facet::new("example", 1)], vec![])
        .unwrap();
    register(next.facets[0].id, vec![]);
    host.commit(next.id, vec![]).unwrap();
    let root = host.begin_root("host").unwrap();
    let call = host
        .begin_operation(
            next.facets[0].id,
            root,
            host.allocate_handle().unwrap(),
            None,
        )
        .unwrap();
    let cleanup = host.cleanup_scopes(vec![previous], None).unwrap();
    host.run_program(cleanup).unwrap();
    for action in host.drain() {
        if let Action::Execute { program, effects } = action {
            complete(&host, program, &effects);
        }
    }
    assert!(host.drain().contains(&Action::Execute {
        program: cleanup,
        effects: vec![Effect::Close {
            resource: resource.id
        }]
    }));
    assert!(host.invocation(call.id).is_ok());
}

#[test]
fn releasing_a_pending_dependency_wakes_its_activation() {
    let host = Host::new();
    let mut facet = Facet::new("dependency", 1);
    facet.services.push(Service::new("dependency", ["read"]));
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let get = || {
        host.acquire(
            Address {
                service: "dependency".into(),
                key: vec!["main".into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap()
    };
    let dependency = get();
    let retained = get();
    let id = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id,
        owner: Owner::Host,
        facet: 0,
        parents: vec![],
        dependencies: vec![dependency.reference],
        resources: vec![],
        state_version: None,
        migrate: false,
        active: false,
        closing: false,
    })
    .unwrap();
    let program = host.activate_scope(id).unwrap();
    host.run_program(program).unwrap();
    assert!(host.drain().is_empty());
    host.release(dependency.reference).unwrap();
    assert_eq!(
        host.status(retained.reference).unwrap(),
        crate::Status::Loading
    );
    let mut pending = VecDeque::from(host.drain());
    let mut failed = false;
    while let Some(action) = pending.pop_front() {
        match action {
            Action::Execute { program, effects } => complete(&host, program, &effects),
            Action::ProgramComplete {
                error: Some(error), ..
            } => {
                assert_eq!(error.reason, "dependency");
                failed = true;
            }
            _ => {}
        }
        pending.extend(host.drain());
    }
    assert!(
        failed,
        "dependency refusal must not await an unrelated retained opener"
    );
}

#[test]
fn failed_activation_closes_its_resources_before_releasing_the_object() {
    let host = Host::new();
    let mut facet = Facet::new("failed", 1);
    facet.services.push(Service::new("failed", ["read"]));
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let address = Address {
        service: "failed".into(),
        key: vec!["main".into()],
        principal: "host".into(),
    };
    let claim = host.acquire(address.clone(), 1, BTreeSet::new()).unwrap();
    let a = host
        .resource(Owner::Object(claim.object), "a".into(), 1, false, vec![])
        .unwrap();
    let b = host
        .resource(
            Owner::Object(claim.object),
            "b".into(),
            1,
            false,
            vec![a.id],
        )
        .unwrap();
    let scope = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id: scope,
        owner: Owner::Object(claim.object),
        facet: claim.facet,
        parents: vec![],
        dependencies: vec![],
        resources: vec![a.id, b.id],
        state_version: None,
        migrate: false,
        active: false,
        closing: false,
    })
    .unwrap();
    let program = host.activate_scope(scope).unwrap();
    host.run_program(program).unwrap();
    let mut pending = VecDeque::from(host.drain());
    let mut closed = vec![];
    let mut failed = false;
    while let Some(action) = pending.pop_front() {
        match action {
            Action::Execute { program, effects } => {
                let mut results = vec![];
                for effect in effects {
                    let error = match effect {
                        Effect::Open { resource } if resource == b.id => {
                            Some(Error::invalid("open_failed", "opener failed"))
                        }
                        Effect::Open { resource } => {
                            host.resource_opened(resource, host.allocate_handle().unwrap())
                                .unwrap();
                            None
                        }
                        Effect::Close { resource } => {
                            closed.push(resource);
                            host.resource_closed(resource).unwrap();
                            None
                        }
                        _ => None,
                    };
                    results.push(Completion { value: None, error });
                }
                host.complete_step(program, results).unwrap();
            }
            Action::DisposeObject(object) => host.disposed(object).unwrap(),
            Action::ProgramComplete {
                error: Some(error), ..
            } => {
                assert_eq!(error.reason, "open_failed");
                failed = true;
            }
            _ => {}
        }
        pending.extend(host.drain());
    }
    assert!(failed);
    assert_eq!(closed, [b.id, a.id]);
    assert_eq!(host.diagnostics().resources, 0);
    assert_eq!(host.diagnostics().objects, 0);
    assert!(host.acquire(address, 1, BTreeSet::new()).unwrap().activate);
}

#[test]
fn prepared_validation_rejects_a_later_write_before_adoption() {
    let host = Host::new();
    let mut facet = Facet::new("state", 1);
    facet.services.push(Service::new("state", ["write"]));
    let initial = host.prepare(vec![facet.clone()], vec![]).unwrap();
    host.commit(initial.id, vec![]).unwrap();
    let claim = host
        .acquire(
            Address {
                service: "state".into(),
                key: vec!["main".into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap();
    host.initialize_state(claim.object, 100, 1).unwrap();
    host.ready(claim.object).unwrap();
    let register = |facet| {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Object(claim.object),
            facet,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version: Some(1),
            migrate: false,
            active: false,
            closing: false,
        })
        .unwrap();
        id
    };
    let previous = register(claim.facet);
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    let next = register(reload.facets[0].id);
    let program = host
        .reload_scopes(
            reload.id,
            vec![Replacement {
                previous: Some(previous),
                next: Some(next),
            }],
            100,
        )
        .unwrap();
    host.run_program(program).unwrap();
    let mut pending = VecDeque::from(host.drain());
    let mut prepared = false;
    while let Some(action) = pending.pop_front() {
        match action {
            Action::Execute { program, effects } => complete(&host, program, &effects),
            Action::ProgramPrepared(_) => prepared = true,
            _ => {}
        }
        pending.extend(host.drain());
    }
    assert!(prepared);
    let root = host.begin_root("host").unwrap();
    let call = host.begin_call(claim.reference, root, "write").unwrap();
    host.adopt_state(call.id, claim.object, 0, 101).unwrap();
    assert_eq!(
        host.commit_program(program).unwrap_err().reason,
        "state_revision"
    );
    assert_eq!(host.snapshot(claim.object).unwrap().value, 101);
    assert_eq!(host.version(), initial.code_version);
}

#[test]
fn concurrent_expiry_visits_only_programs_with_work() {
    let host = Host::new();
    let mut facet = Facet::new("expiry", 1);
    let mut service = Service::new("expiry.item", ["read"]);
    service.linger_ms = 1_000;
    facet.services.push(service);
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let count = 200;
    let mut resources = BTreeMap::new();
    let mut scopes = BTreeMap::new();
    for index in 0..count {
        let claim = host
            .acquire(
                Address {
                    service: "expiry.item".into(),
                    key: vec![index.to_string()],
                    principal: "host".into(),
                },
                1,
                BTreeSet::new(),
            )
            .unwrap();
        let resource = host
            .resource(
                Owner::Object(claim.object),
                "resource".into(),
                1,
                false,
                vec![],
            )
            .unwrap();
        host.resource_opened(resource.id, host.allocate_handle().unwrap())
            .unwrap();
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Object(claim.object),
            facet: claim.facet,
            parents: vec![],
            dependencies: vec![],
            resources: vec![resource.id],
            state_version: None,
            migrate: false,
            active: true,
            closing: false,
        })
        .unwrap();
        scopes.insert(id, resource.id);
        resources.insert(resource.id, 0);
        host.ready(claim.object).unwrap();
        host.release(claim.reference).unwrap();
    }
    host.tick(1_000).unwrap();
    let mut pending = VecDeque::from(host.drain());
    while let Some(action) = pending.pop_front() {
        match action {
            Action::DisposeObject(object) => {
                let program = host
                    .cleanup_scopes(host.scopes_for_owner(Owner::Object(object)), Some(object))
                    .unwrap();
                host.run_program(program).unwrap();
            }
            Action::Execute { program, effects } => {
                for effect in &effects {
                    match effect {
                        Effect::Close { resource } => {
                            *resources.get_mut(resource).unwrap() += 1;
                            host.resource_closed(*resource).unwrap();
                        }
                        Effect::Drop { scope } => assert_eq!(resources[&scopes[scope]], 1),
                        _ => {}
                    }
                }
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
            Action::ProgramComplete { error, .. } => assert!(error.is_none(), "{error:?}"),
            _ => {}
        }
        pending.extend(host.drain());
    }
    assert!(resources.values().all(|count| *count == 1));
    assert_eq!(host.diagnostics().objects, 0);
    assert_eq!(host.diagnostics().resources, 0);
    assert!(host.inner.borrow().programs.is_empty());
    let visits = host.inner.borrow().program_visits;
    assert!(
        visits <= count * 8,
        "{visits} program visits for {count} independent closures"
    );
}
