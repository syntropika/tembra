use tembra_core::*;

fn scope(host: &Host, owner: Owner, facet: Id, resources: Vec<Id>) -> Id {
    let id = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id,
        owner,
        facet,
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
    let installed = host
        .prepare(vec![Facet::new("example", 1)], vec![])
        .unwrap();
    host.commit(installed.id, vec![]).unwrap();
    let old = scope(
        &host,
        Owner::Facet(installed.facets[0].owner),
        installed.facets[0].id,
        vec![],
    );
    let reload = host
        .prepare(vec![Facet::new("example", 1)], vec![])
        .unwrap();
    let resource = host
        .resource(
            Owner::Facet(reload.facets[0].owner),
            "connection".into(),
            1,
            false,
            vec![],
        )
        .unwrap();
    let next = scope(
        &host,
        Owner::Facet(reload.facets[0].owner),
        reload.facets[0].id,
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
