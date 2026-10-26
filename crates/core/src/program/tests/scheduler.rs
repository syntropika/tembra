use super::*;

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
fn concurrent_expiry_visits_only_programs_with_work() {
    let host = Host::new();
    let mut orb = Orb::new("expiry", 1);
    let mut service = Service::new("expiry.item", ["read"]);
    service.linger_ms = 1_000;
    orb.services.push(service);
    let reload = host.prepare(vec![orb], vec![]).unwrap();
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
            orb: claim.orb,
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
