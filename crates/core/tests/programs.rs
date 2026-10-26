use tembra_core::*;

fn scope(host: &Host, owner: Owner, orb: Id, resources: Vec<Id>) -> Id {
    let id = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id,
        owner,
        orb,
        resources,
        parents: vec![],
        dependencies: vec![],
        state_version: None,
        migrate: false,
        active: false,
        closing: false,
    })
    .unwrap();
    id
}

fn effects(host: &Host, program: Id) -> Vec<Effect> {
    host.drain()
        .into_iter()
        .find_map(|action| match action {
            Action::Execute {
                program: id,
                effects,
            } if id == program => Some(effects),
            _ => None,
        })
        .expect("engine requested execution")
}

fn acknowledge(host: &Host, program: Id, effects: &[Effect]) {
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
fn kernel_orders_resource_opening_and_closing_without_an_adapter_planner() {
    let host = Host::new();
    let base = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    let dependent = host
        .resource(Owner::Host, "dependent".into(), 1, false, vec![base.id])
        .unwrap();
    let scope = scope(&host, Owner::Host, 0, vec![dependent.id, base.id]);
    let program = host.activate_scope(scope).unwrap();
    host.run_program(program).unwrap();
    assert_eq!(
        effects(&host, program),
        vec![Effect::Open { resource: base.id }]
    );
    host.resource_opened(base.id, 100).unwrap();
    acknowledge(&host, program, &[Effect::Open { resource: base.id }]);
    assert_eq!(
        effects(&host, program),
        vec![Effect::Open {
            resource: dependent.id
        }]
    );
    host.resource_opened(dependent.id, 101).unwrap();
    acknowledge(
        &host,
        program,
        &[Effect::Open {
            resource: dependent.id,
        }],
    );
    let prepare = effects(&host, program);
    assert_eq!(
        prepare,
        vec![Effect::Prepare {
            scope,
            previous: None
        }]
    );
    acknowledge(&host, program, &prepare);
    let start = effects(&host, program);
    assert_eq!(start, vec![Effect::Start { scope }]);
    acknowledge(&host, program, &start);
    assert!(host.drain().contains(&Action::ProgramComplete {
        program,
        error: None
    }));

    let cleanup = host.cleanup_scopes(vec![scope], None).unwrap();
    host.run_program(cleanup).unwrap();
    let stop = effects(&host, cleanup);
    acknowledge(&host, cleanup, &stop);
    for resource in [dependent.id, base.id] {
        assert_eq!(effects(&host, cleanup), vec![Effect::Close { resource }]);
        host.resource_closed(resource).unwrap();
        acknowledge(&host, cleanup, &[Effect::Close { resource }]);
    }
    let drop = effects(&host, cleanup);
    assert_eq!(drop, vec![Effect::Drop { scope }]);
    acknowledge(&host, cleanup, &drop);
    assert!(host.drain().contains(&Action::ProgramComplete {
        program: cleanup,
        error: None
    }));
    assert_eq!(host.diagnostics().resources, 0);
}

#[test]
fn a_reload_deadline_requests_cancellation_but_waits_for_physical_settlement() {
    let host = Host::new();
    let installed = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    host.commit(installed.id, vec![]).unwrap();
    let old = scope(
        &host,
        Owner::Orb(installed.orbs[0].owner),
        installed.orbs[0].id,
        vec![],
    );
    let reload = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    let resource = host
        .resource(
            Owner::Orb(reload.orbs[0].owner),
            "connection".into(),
            1,
            false,
            vec![],
        )
        .unwrap();
    let next = scope(
        &host,
        Owner::Orb(reload.orbs[0].owner),
        reload.orbs[0].id,
        vec![resource.id],
    );
    let program = host
        .reload_scopes(
            reload.id,
            vec![Replacement {
                previous: Some(old),
                next: Some(next),
            }],
            5,
        )
        .unwrap();
    host.run_program(program).unwrap();
    let stop = effects(&host, program);
    acknowledge(&host, program, &stop);
    let open = effects(&host, program);
    assert_eq!(
        open,
        vec![Effect::Open {
            resource: resource.id
        }]
    );
    host.tick(5).unwrap();
    let actions = host.drain();
    assert!(actions.contains(&Action::CancelEffects {
        program,
        effects: open.clone()
    }));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::ProgramComplete { .. }))
    );
    assert_eq!(host.diagnostics().resources, 1);
    host.complete_step(
        program,
        vec![Completion {
            value: None,
            error: Some(Error::new(
                Code::Cancelled,
                "open_cancelled",
                "opening settled",
            )),
        }],
    )
    .unwrap();
    let stop = effects(&host, program);
    acknowledge(&host, program, &stop);
    assert_eq!(
        effects(&host, program),
        vec![Effect::Close {
            resource: resource.id
        }]
    );
    host.resource_closed(resource.id).unwrap();
    acknowledge(
        &host,
        program,
        &[Effect::Close {
            resource: resource.id,
        }],
    );
    let prepare = effects(&host, program);
    acknowledge(&host, program, &prepare);
    let start = effects(&host, program);
    acknowledge(&host, program, &start);
    let drop = effects(&host, program);
    acknowledge(&host, program, &drop);
    let actions = host.drain();
    assert!(actions.iter().any(|action| matches!(action, Action::ProgramComplete { program: id, error: Some(error) } if *id == program && error.reason == "reload_deadline")));
    assert!(host.scope_exists(old));
    assert!(!host.scope_exists(next));
    assert_eq!(host.diagnostics().resources, 0);
}

#[test]
fn close_cancels_reload_before_cleaning_its_scopes() {
    let host = Host::new();
    let installed = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    host.commit(installed.id, vec![]).unwrap();
    let previous = scope(
        &host,
        Owner::Orb(installed.orbs[0].owner),
        installed.orbs[0].id,
        vec![],
    );
    let reload = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    let resource = host
        .resource(
            Owner::Orb(reload.orbs[0].owner),
            "connection".into(),
            1,
            false,
            vec![],
        )
        .unwrap();
    let next = scope(
        &host,
        Owner::Orb(reload.orbs[0].owner),
        reload.orbs[0].id,
        vec![resource.id],
    );
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
    let stop = effects(&host, program);
    acknowledge(&host, program, &stop);
    let opening = effects(&host, program);
    assert_eq!(
        opening,
        vec![Effect::Open {
            resource: resource.id
        }]
    );

    host.close();
    let mut actions = host.drain();
    assert!(actions.contains(&Action::CancelEffects {
        program,
        effects: opening
    }));
    assert!(host.scope_exists(previous) && host.scope_exists(next));
    assert_eq!(host.finish_close().unwrap_err().reason, "cleanup_busy");
    // A cancellation request cannot acknowledge a still-running opener.
    assert!(!actions.iter().any(|action| matches!(action, Action::Execute { effects, .. } if effects.iter().any(|effect| matches!(effect, Effect::Close { .. } | Effect::Drop { .. })))));
    host.resource_opened(resource.id, host.allocate_handle().unwrap())
        .unwrap();
    acknowledge(
        &host,
        program,
        &[Effect::Open {
            resource: resource.id,
        }],
    );
    let mut completed = false;
    let mut closed = 0;
    for _ in 0..32 {
        actions.extend(host.drain());
        for action in std::mem::take(&mut actions) {
            match action {
                Action::Execute {
                    program: id,
                    effects,
                } => {
                    for effect in &effects {
                        if let Effect::Close { resource } = effect {
                            closed += 1;
                            host.resource_closed(*resource).unwrap();
                        }
                        assert!(
                            !matches!(effect, Effect::Start { .. } | Effect::Restore { .. }),
                            "shutdown must not restart scopes: {effect:?}"
                        );
                    }
                    acknowledge(&host, id, &effects);
                }
                Action::ProgramComplete { program: id, error } if id == program => {
                    assert_eq!(error.unwrap().reason, "reload_aborted");
                    completed = true;
                }
                Action::ProgramComplete { error, .. } => assert!(error.is_none(), "{error:?}"),
                _ => {}
            }
        }
        if host.finish_close().is_ok() {
            assert!(completed);
            assert_eq!(closed, 1);
            assert!(!host.scope_exists(previous) && !host.scope_exists(next));
            return;
        }
    }
    panic!("shutdown did not settle");
}

#[test]
fn close_finishes_committed_adoption_before_cleanup_without_starting_activities() {
    let host = Host::new();
    let reload = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    let next = scope(
        &host,
        Owner::Orb(reload.orbs[0].owner),
        reload.orbs[0].id,
        vec![],
    );
    let program = host
        .reload_scopes(
            reload.id,
            vec![Replacement {
                previous: None,
                next: Some(next),
            }],
            100,
        )
        .unwrap();
    host.run_program(program).unwrap();
    let prepare = effects(&host, program);
    acknowledge(&host, program, &prepare);
    assert!(host.drain().contains(&Action::ProgramPrepared(program)));
    host.commit_program(program).unwrap();
    let adopt = effects(&host, program);
    assert!(matches!(adopt.as_slice(), [Effect::Adopt { .. }]));
    host.close();
    let actions = host.drain();
    assert!(!actions.iter().any(|action| matches!(
        action,
        Action::CancelEffects { .. } | Action::Execute { .. }
    )));
    assert!(host.scope_exists(next));
    acknowledge(&host, program, &adopt);
    let mut completed = false;
    for _ in 0..16 {
        for action in host.drain() {
            match action {
                Action::Execute { program, effects } => {
                    assert!(
                        !effects
                            .iter()
                            .any(|effect| matches!(effect, Effect::Start { .. }))
                    );
                    acknowledge(&host, program, &effects);
                }
                Action::ProgramComplete { program: id, error } => {
                    assert!(error.is_none(), "{error:?}");
                    completed |= id == program;
                }
                _ => {}
            }
        }
        if host.finish_close().is_ok() {
            assert!(completed);
            assert_eq!(host.version(), reload.code_version);
            return;
        }
    }
    panic!("committed shutdown did not settle");
}
