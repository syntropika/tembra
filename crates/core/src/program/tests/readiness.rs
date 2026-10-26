use super::*;

#[test]
fn external_resource_readiness_wakes_only_its_waiting_activation() {
    let host = Host::new();
    let waiting: Vec<_> = (0..32)
        .map(|index| waiting_resource(&host, &index.to_string()))
        .collect();
    assert!(host.drain().is_empty());
    let visits = host.inner.borrow().program_visits;
    for _ in 0..100 {
        assert!(host.drain().is_empty());
    }
    assert_eq!(host.inner.borrow().program_visits, visits);
    let unrelated = host
        .resource(Owner::Host, "unrelated".into(), 1, false, vec![])
        .unwrap();
    host.resource_opened(unrelated.id, host.allocate_handle().unwrap())
        .unwrap();
    assert!(host.drain().is_empty());
    assert_eq!(host.inner.borrow().program_visits, visits);
    let (base, dependent, program) = waiting[0];
    host.resource_opened(base, host.allocate_handle().unwrap())
        .unwrap();
    assert_eq!(
        host.drain(),
        [Action::Execute {
            program,
            effects: vec![Effect::Open {
                resource: dependent
            }]
        }]
    );
    assert_eq!(host.inner.borrow().program_visits, visits + 1);
    host.resource_opened(dependent, host.allocate_handle().unwrap())
        .unwrap();
    complete(
        &host,
        program,
        &[Effect::Open {
            resource: dependent,
        }],
    );
    let mut completed = false;
    for _ in 0..8 {
        for action in host.drain() {
            match action {
                Action::Execute {
                    program: id,
                    effects,
                } => {
                    assert_eq!(id, program);
                    complete(&host, id, &effects);
                }
                Action::ProgramComplete { program: id, error } => {
                    assert_eq!(id, program);
                    assert!(error.is_none());
                    completed = true;
                }
                other => panic!("unexpected action: {other:?}"),
            }
        }
    }
    assert!(completed);
    let inner = host.inner.borrow();
    assert_eq!(inner.programs.len(), waiting.len() - 1);
    assert!(!inner.waiters.contains(&WaitKey::Resource(base)));
}

#[test]
fn closing_an_unopened_provider_wakes_and_rejects_its_waiting_consumer() {
    let host = Host::new();
    let (base, _, consumer) = waiting_resource(&host, "closing");
    let provider_scope = host_scope(&host, vec![base], vec![]);
    assert!(host.drain().is_empty());
    let cleanup = host.cleanup_scopes(vec![provider_scope], None).unwrap();
    host.run_program(cleanup).unwrap();
    settle_failed_consumer(&host, consumer);
    assert_eq!(host.diagnostics().resources, 0);
    host.close();
    host.finish_close().unwrap();
}

#[test]
fn retirement_wakes_a_consumer_even_when_another_dependency_is_still_pending() {
    let host = Host::new();
    let base = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    host.resource_opened(base.id, host.allocate_handle().unwrap())
        .unwrap();
    let provider = host_scope(&host, vec![base.id], vec![]);
    let blocker = host
        .resource(Owner::Host, "blocker".into(), 1, false, vec![])
        .unwrap();
    let dependent = host
        .resource(
            Owner::Host,
            "consumer".into(),
            1,
            false,
            vec![base.id, blocker.id],
        )
        .unwrap();
    let scope = host_scope(&host, vec![dependent.id], vec![]);
    host.inner
        .borrow_mut()
        .scopes
        .get_mut(&scope)
        .unwrap()
        .active = false;
    let consumer = host.activate_scope(scope).unwrap();
    host.run_program(consumer).unwrap();
    assert!(host.drain().is_empty());
    assert!(
        host.inner
            .borrow()
            .waiters
            .contains(&WaitKey::Resource(base.id))
    );
    let cleanup = host.cleanup_scopes(vec![provider], None).unwrap();
    host.run_program(cleanup).unwrap();
    settle_failed_consumer(&host, consumer);
    assert!(host.close_resource(blocker.id).unwrap());
    host.resource_closed(blocker.id).unwrap();
    host.close();
    host.drain();
    host.finish_close().unwrap();
}

#[test]
fn releasing_a_pending_dependency_wakes_its_activation() {
    let host = Host::new();
    let mut orb = Orb::new("dependency", 1);
    orb.services.push(Service::new("dependency", ["read"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
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
        orb: 0,
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
