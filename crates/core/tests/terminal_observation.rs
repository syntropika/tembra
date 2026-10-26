#![cfg(feature = "hooks")]
use std::collections::BTreeSet;
use tembra_core::*;

fn install(host: &Host, name: &str) -> InstalledOrb {
    let mut declaration = Orb::new(name, 1);
    declaration.services.push(Service::new(name, ["run"]));
    declaration.hooks = ["terminal".into(), "other".into()].into();
    let reload = host.prepare(vec![declaration], vec![]).unwrap();
    let orb = reload.orbs[0].clone();
    host.commit(reload.id, vec![]).unwrap();
    scope(host, Owner::Orb(orb.owner), orb.id, vec![]);
    orb
}

fn scope(host: &Host, owner: Owner, orb: Id, parents: Vec<Id>) -> Id {
    let id = host.allocate_handle().unwrap();
    host.register_scope(Scope {
        id,
        owner,
        orb,
        parents,
        dependencies: vec![],
        resources: vec![],
        state_version: None,
        migrate: false,
        active: true,
        closing: false,
    })
    .unwrap();
    id
}

fn entered(host: &Host, orb: &InstalledOrb) -> Invocation {
    let root = host.begin_root("alice").unwrap();
    host.begin_operation(orb.id, root, host.allocate_handle().unwrap(), None)
        .unwrap()
}

fn object_source(host: &Host, orb: &InstalledOrb) -> (Invocation, Id, Id) {
    let claim = host
        .acquire(
            Address {
                service: orb.declaration.name.clone(),
                key: vec!["main".into()],
                principal: "alice".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap();
    host.ready(claim.object).unwrap();
    let owner_scope = scope(host, Owner::Object(claim.object), orb.id, vec![]);
    let root = host.begin_root("alice").unwrap();
    let source = host.begin_call(claim.reference, root, "run").unwrap();
    (source, owner_scope, claim.reference)
}

fn observe(host: &Host, source: &Invocation) -> Result<HookObservation> {
    host.begin_hook_observation(
        source.id,
        source.orb,
        host.allocate_handle().unwrap(),
        None,
        "terminal",
    )
}

fn child(host: &Host, orb: &InstalledOrb, root: Id) -> Invocation {
    host.begin_operation(orb.id, root, host.allocate_handle().unwrap(), None)
        .unwrap()
}

fn finish(host: &Host, call: Invocation) {
    host.end_root(call.root).unwrap();
    host.finish_call(call.id).unwrap();
}

fn drive(host: &Host) -> Vec<Action> {
    let mut all = Vec::new();
    loop {
        let actions = host.drain();
        if actions.is_empty() {
            break;
        }
        for action in &actions {
            if let Action::Execute { program, effects } = action {
                host.complete_step(
                    *program,
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
        }
        all.extend(actions);
    }
    all
}

fn cleanup(host: &Host, scopes: Vec<Id>) -> Vec<Action> {
    let program = host.cleanup_scopes(scopes, None).unwrap();
    host.run_program(program).unwrap();
    drive(host)
}

fn assert_settled(host: &Host) {
    assert_eq!(host.diagnostics().roots, 0);
    assert_eq!(host.diagnostics().calls, 0);
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn cancelled_and_ended_sources_keep_principal_version_and_empty_frame() {
    for ended in [false, true] {
        let host = Host::new();
        let orb = install(&host, "provider");
        let source = entered(&host, &orb);
        let key: ContextKey<String> = ContextKey::new("test/trace", 1);
        let frame = host
            .derive_context(
                source.root,
                0,
                Some(source.id),
                key.contract(),
                "trace".to_string().into_value(),
            )
            .unwrap();
        host.cancel_root(source.root).unwrap();
        if ended {
            host.end_root(source.root).unwrap();
        }
        let observation = observe(&host, &source).unwrap();
        assert!(observation.plan.is_empty());
        let terminal = observation.invocation;
        assert_eq!(terminal.context, 0);
        assert_eq!(terminal.code_version, source.code_version);
        assert_eq!(host.root_principal(terminal.root).unwrap(), "alice");
        assert_eq!(
            host.context_value(terminal.root, 0, &key.contract())
                .unwrap(),
            None
        );
        assert!(
            host.context_value(terminal.root, frame, &key.contract())
                .is_err()
        );
        assert!(host.check_root(source.root).is_err());
        finish(&host, terminal);
        if !ended {
            host.end_root(source.root).unwrap();
        }
        host.finish_call(source.id).unwrap();
        assert_eq!(observe(&host, &source).unwrap_err().reason, "call");
        assert_settled(&host);
    }
}

#[test]
fn one_successful_admission_is_shared_across_all_hook_handles() {
    let host = Host::new();
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    finish(&host, terminal);
    let before = host.diagnostics();
    assert_eq!(
        host.begin_hook_observation(
            source.id,
            orb.id,
            host.allocate_handle().unwrap(),
            None,
            "other"
        )
        .unwrap_err()
        .reason,
        "observation_admitted"
    );
    assert_eq!(host.diagnostics(), before);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn observation_and_descendant_invocations_cannot_reopen_observation() {
    let host = Host::new();
    let dependency = install(&host, "dependency");
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    let descendant = child(&host, &dependency, terminal.root);
    for call in [&terminal, &descendant] {
        assert_eq!(
            observe(&host, call).unwrap_err().reason,
            "observation_source"
        );
    }
    host.finish_call(descendant.id).unwrap();
    finish(&host, terminal);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn wrong_owner_and_invalid_hook_refusals_do_not_consume_the_source() {
    let host = Host::new();
    let foreign = install(&host, "foreign");
    let orb = install(&host, "provider");
    let (source, _, _) = object_source(&host, &orb);
    let before = host.diagnostics();
    let operation = host.allocate_handle().unwrap();
    let error = host
        .begin_hook_observation(source.id, foreign.id, operation, None, "terminal")
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Forbidden, "operation_owner")
    );
    assert_eq!(
        host.begin_hook_observation(
            source.id,
            orb.id,
            operation,
            Some(source.object + 1),
            "terminal"
        )
        .unwrap_err()
        .reason,
        "operation_owner"
    );
    assert_eq!(
        host.begin_hook_observation(source.id, orb.id, operation, Some(source.object), "missing")
            .unwrap_err()
            .reason,
        "hook"
    );
    assert_eq!(
        host.begin_hook_observation(source.id, orb.id, 0, None, "terminal")
            .unwrap_err()
            .reason,
        "operation"
    );
    assert_eq!(host.diagnostics(), before);
    let terminal = observe(&host, &source).unwrap().invocation;
    // Orb-bound handles still retain the actual physical object owner.
    assert_eq!(terminal.object, source.object);
    finish(&host, terminal);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn expired_or_invented_source_calls_retain_nothing() {
    let host = Host::new();
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    finish(&host, source.clone());
    let before = host.diagnostics();
    assert_eq!(observe(&host, &source).unwrap_err().reason, "call");
    assert_eq!(
        host.begin_hook_observation(0, orb.id, host.allocate_handle().unwrap(), None, "terminal")
            .unwrap_err()
            .reason,
        "call"
    );
    assert_eq!(host.diagnostics(), before);
}

#[test]
fn closing_and_removed_exact_source_scopes_refuse_new_observation() {
    let host = Host::new();
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let id = host.scopes_for_orb(orb.id)[0];
    let program = host.cleanup_scopes(vec![id], None).unwrap();
    assert_eq!(observe(&host, &source).unwrap_err().reason, "scope_closed");
    host.run_program(program).unwrap();
    drive(&host);
    assert!(!host.scope_exists(id));
    assert_eq!(observe(&host, &source).unwrap_err().reason, "scope");
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn closing_host_ancestry_refuses_admission_but_unrelated_host_scopes_do_not() {
    for ancestor in [false, true] {
        let host = Host::new();
        let host_scope = scope(&host, Owner::Host, 0, vec![]);
        let orb = install(&host, "provider");
        if ancestor {
            scope(&host, Owner::Orb(orb.owner), orb.id, vec![host_scope]);
        }
        let source = entered(&host, &orb);
        host.cleanup_scopes(vec![host_scope], None).unwrap();
        if ancestor {
            assert_eq!(observe(&host, &source).unwrap_err().reason, "scope_closed");
        } else {
            let terminal = observe(&host, &source).unwrap().invocation;
            finish(&host, terminal);
        }
        finish(&host, source);
        assert_settled(&host);
    }
}

#[test]
fn completed_temporary_dependency_cleanup_neither_cancels_nor_retains_observation() {
    let host = Host::new();
    let dependency = install(&host, "dependency");
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    let dependent = child(&host, &dependency, terminal.root);
    host.finish_call(dependent.id).unwrap();
    let dependency_scope = host.scopes_for_orb(dependency.id)[0];
    let actions = cleanup(&host, vec![dependency_scope]);
    assert!(!actions.contains(&Action::CancelRoot(terminal.root)));
    assert!(!host.scope_exists(dependency_scope));
    host.check_root(terminal.root).unwrap();
    let later = child(&host, &orb, terminal.root);
    host.finish_call(later.id).unwrap();
    finish(&host, terminal);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn active_dependency_cleanup_cancels_and_waits_only_its_physical_call_claim() {
    let host = Host::new();
    let dependency = install(&host, "dependency");
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    let dependent = child(&host, &dependency, terminal.root);
    let id = host.scopes_for_orb(dependency.id)[0];
    let actions = cleanup(&host, vec![id]);
    assert!(actions.contains(&Action::CancelRoot(terminal.root)));
    assert!(host.scope_exists(id));
    assert_eq!(
        host.check_root(terminal.root).unwrap_err().code,
        Code::Cancelled
    );
    host.finish_call(dependent.id).unwrap();
    drive(&host);
    assert!(!host.scope_exists(id));
    // The observation itself is still physically retained by its source anchor.
    assert!(host.invocation(terminal.id).is_ok());
    finish(&host, terminal);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn source_object_anchor_survives_a_detached_descendant_in_another_orb() {
    let host = Host::new();
    let dependency = install(&host, "dependency");
    let orb = install(&host, "provider");
    let (source, source_scope, _) = object_source(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    let dependent = child(&host, &dependency, terminal.root);
    finish(&host, terminal.clone());
    let actions = cleanup(&host, vec![source_scope]);
    assert!(actions.contains(&Action::CancelRoot(terminal.root)));
    assert!(host.scope_exists(source_scope));
    host.finish_call(dependent.id).unwrap();
    drive(&host);
    assert!(!host.scope_exists(source_scope));
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn a_fixed_plan_uses_the_sources_exact_version_after_publication() {
    let host = Host::new();
    let old = install(&host, "provider");
    let old_operation = host.allocate_handle().unwrap();
    host.contribute(Contribution {
        orb: old.id,
        target: "provider".into(),
        hook: "terminal".into(),
        operation: old_operation,
        priority: 0,
        ordinal: host.contribution_ordinal().unwrap(),
    })
    .unwrap();
    let source = entered(&host, &old);
    let current = install(&host, "provider");
    let new_operation = host.allocate_handle().unwrap();
    host.contribute(Contribution {
        orb: current.id,
        target: "provider".into(),
        hook: "terminal".into(),
        operation: new_operation,
        priority: 0,
        ordinal: host.contribution_ordinal().unwrap(),
    })
    .unwrap();
    let observation = observe(&host, &source).unwrap();
    assert_eq!(&*observation.plan, &[old_operation]);
    assert_eq!(observation.invocation.code_version, source.code_version);
    assert_ne!(source.code_version, current.code_version);
    finish(&host, observation.invocation);
    finish(&host, source);
    assert_settled(&host);
}

#[test]
fn orb_and_object_gates_refuse_without_consuming_or_retaining_claims() {
    for object_bound in [false, true] {
        let host = Host::new();
        let orb = install(&host, "provider");
        let source = if object_bound {
            object_source(&host, &orb).0
        } else {
            entered(&host, &orb)
        };
        let mut next = Orb::new("provider", 1);
        next.services.push(Service::new("provider", ["run"]));
        next.hooks.insert("terminal".into());
        let reload = host.prepare(vec![next], vec![]).unwrap();
        if object_bound {
            assert!(!host.gate(reload.id, &[source.object]).unwrap());
        } else {
            assert!(!host.gate_orbs(reload.id, &[orb.owner]).unwrap());
        }
        let before = host.diagnostics();
        assert_eq!(
            observe(&host, &source).unwrap_err().reason,
            "upgrade_conflict"
        );
        assert_eq!(host.diagnostics(), before);
        host.abort(reload.id).unwrap();
        let terminal = observe(&host, &source).unwrap().invocation;
        finish(&host, terminal);
        finish(&host, source);
        assert_settled(&host);
    }
}

#[test]
fn host_shutdown_cancels_observation_refuses_admission_and_waits_for_settlement() {
    let host = Host::new();
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    host.close();
    assert_eq!(observe(&host, &source).unwrap_err().code, Code::Closed);
    assert_eq!(
        host.check_root(terminal.root).unwrap_err().code,
        Code::Cancelled
    );
    assert_eq!(host.finish_close().unwrap_err().reason, "cleanup_busy");
    finish(&host, terminal);
    finish(&host, source);
    drive(&host);
    host.finish_close().unwrap();
    assert_settled(&host);
    assert_eq!(host.diagnostics().resources, 0);
}

#[test]
fn dependent_scope_validation_is_observation_only_and_refusal_retains_nothing() {
    let host = Host::new();
    let dependency = install(&host, "dependency");
    let orb = install(&host, "provider");
    let source = entered(&host, &orb);
    let terminal = observe(&host, &source).unwrap().invocation;
    let id = host.scopes_for_orb(dependency.id)[0];
    cleanup(&host, vec![id]);
    let before = host.diagnostics();
    assert_eq!(
        host.begin_operation(
            dependency.id,
            terminal.root,
            host.allocate_handle().unwrap(),
            None
        )
        .unwrap_err()
        .reason,
        "scope"
    );
    assert_eq!(host.diagnostics(), before);
    let ordinary = child(&host, &dependency, source.root);
    host.finish_call(ordinary.id).unwrap();
    finish(&host, terminal);
    finish(&host, source);
    assert_settled(&host);
}
