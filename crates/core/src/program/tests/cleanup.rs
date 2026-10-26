use super::*;

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
