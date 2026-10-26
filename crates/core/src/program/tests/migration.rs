use super::*;

#[test]
fn prepared_validation_rejects_a_later_write_before_adoption() {
    let host = Host::new();
    let mut orb = Orb::new("state", 1);
    orb.services.push(Service::new("state", ["write"]));
    let initial = host.prepare(vec![orb.clone()], vec![]).unwrap();
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
    let register = |orb| {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Object(claim.object),
            orb,
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
    let previous = register(claim.orb);
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    let next = register(reload.orbs[0].id);
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
fn invalid_state_completion_recovers_and_drops_other_candidates() {
    let host = Host::new();
    let mut orb = Orb::new("state", 1);
    orb.services.push(Service::new("state", ["read"]));
    let initial = host.prepare(vec![orb.clone()], vec![]).unwrap();
    host.commit(initial.id, vec![]).unwrap();
    let claims: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|key| {
            let claim = host
                .acquire(
                    Address {
                        service: "state".into(),
                        key: vec![key.into()],
                        principal: "host".into(),
                    },
                    1,
                    BTreeSet::new(),
                )
                .unwrap();
            host.ready(claim.object).unwrap();
            claim
        })
        .collect();
    let register = |object, orb, state_version| {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Object(object),
            orb,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version,
            migrate: false,
            active: state_version.is_none(),
            closing: false,
        })
        .unwrap();
        id
    };
    let previous: Vec<_> = claims
        .iter()
        .map(|claim| register(claim.object, claim.orb, None))
        .collect();
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    let replacements = claims
        .iter()
        .zip(previous.iter())
        .map(|(claim, previous)| Replacement {
            previous: Some(*previous),
            next: Some(register(claim.object, reload.orbs[0].id, Some(1))),
        })
        .collect();
    let program = host.reload_scopes(reload.id, replacements, 100).unwrap();
    host.run_program(program).unwrap();
    let candidate = host.allocate_handle().unwrap();
    let mut initialized = false;
    let mut dropped = false;
    let mut failed = false;
    for _ in 0..32 {
        for action in host.drain() {
            match action {
                Action::Execute { program, effects } => {
                    if effects
                        .iter()
                        .any(|effect| matches!(effect, Effect::Initialize { .. }))
                    {
                        assert_eq!(effects.len(), 2);
                        assert!(!initialized);
                        initialized = true;
                        host.complete_step(
                            program,
                            vec![
                                Completion {
                                    value: None,
                                    error: None,
                                },
                                Completion {
                                    value: Some(candidate),
                                    error: None,
                                },
                            ],
                        )
                        .unwrap();
                    } else {
                        complete(&host, program, &effects);
                    }
                }
                Action::DropValue(value) if value == candidate => dropped = true,
                Action::ProgramComplete { program: id, error } if id == program => {
                    assert_eq!(error.unwrap().reason, Reason::MigrationValue.as_str());
                    failed = true;
                }
                Action::ProgramPrepared(_) => panic!("invalid state cannot prepare"),
                _ => {}
            }
        }
    }
    assert!(initialized && dropped && failed);
    assert!(host.inner.borrow().programs.is_empty());
    assert_eq!(host.version(), initial.code_version);
    for claim in claims {
        assert_eq!(
            host.snapshot(claim.object).unwrap_err().reason,
            Reason::State.as_str()
        );
    }
    for scope in previous {
        assert!(host.inner.borrow().scopes[&scope].active);
    }
}
