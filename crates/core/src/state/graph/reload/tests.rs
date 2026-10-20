use super::*;
use futures::FutureExt;
use serde_json::json;
fn graph() -> Rc<StateGraph> {
    StateGraph::new(
        Rc::new(|| Ok(())),
        Rc::new(|| Ok(())),
        Rc::new(|ctx| ctx.check(1)),
        Rc::new(|_| {}),
        PublicationQueue::new(),
    )
}
fn plan(graph: &Rc<StateGraph>) -> (Rc<GraphReload>, Rc<Flag<bool>>) {
    let active = Rc::new(Flag::new(true));
    let admitted = active.clone();
    let plan = graph
        .begin_reload(Rc::new(move || {
            if admitted.get() {
                Ok(())
            } else {
                Err(RuntimeError::invalid("declaration_scope", "pass ended"))
            }
        }))
        .unwrap();
    (plan, active)
}
fn cell(graph: &Rc<StateGraph>, name: &str, value: Value) -> Cell {
    graph
        .declare_cell(
            name.into(),
            StateDeclaration::new(json!({"type":"integer"}), value),
        )
        .unwrap()
}
#[test]
fn failed_migration_keeps_installed_value_metadata_revision_and_observers_unchanged() {
    let graph = graph();
    let value = cell(&graph, "value", json!(2));
    graph.activate().unwrap();
    let before = value.snapshot().unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    let _subscription = value
        .readable()
        .subscribe_events(move |event| {
            seen.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    let (plan, _) = plan(&graph);
    let same = plan
        .declare_cell(
            "value".into(),
            StateDeclaration::new(json!({"type":"integer"}), json!(999))
                .version(4)
                .with_migrate(|old, version| {
                    assert_eq!(old, json!(2));
                    assert_eq!(version, 1);
                    Ok(json!("bad"))
                }),
        )
        .unwrap();
    assert!(Rc::ptr_eq(&value.metadata, &same.metadata));
    assert!(plan.requires_gate());
    let sources = CandidateSources::new(vec![plan.clone()]);
    assert!(sources.prepare_all().is_err());
    assert_eq!(value.version(), 1);
    assert_eq!(value.schema().as_ref(), &json!({"type":"integer"}));
    assert_eq!(value.snapshot().unwrap(), before);
    assert_eq!(events.borrow().len(), 1);
    plan.abort().finish();
    assert_eq!(value.snapshot().unwrap(), before);
}
#[test]
fn jump_migration_and_schema_reset_adopt_together_after_private_preparation() {
    let graph = graph();
    let value = cell(&graph, "value", json!(2));
    graph.activate().unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    let _subscription = value
        .readable()
        .subscribe_events(move |event| {
            seen.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    let (plan, _) = plan(&graph);
    plan.declare_cell(
        "value".into(),
        StateDeclaration::new(json!({"type":"string"}), json!("ignored"))
            .version(5)
            .with_migrate(|old, from| {
                assert_eq!(from, 1);
                Ok(json!(old.as_i64().unwrap().to_string()))
            }),
    )
    .unwrap();
    let sources = CandidateSources::new(vec![plan]);
    let mut prepared = sources.prepare_all().unwrap().pop().unwrap();
    assert_eq!(value.version(), 1);
    assert_eq!(events.borrow().len(), 1);
    prepared.preflight().unwrap();
    let queue = graph.queue.clone();
    let hold = queue.hold();
    queue.append_batch(prepared.adopt());
    assert_eq!(value.version(), 5);
    assert_eq!(value.schema().as_ref(), &json!({"type":"string"}));
    prepared.finish();
    drop(hold);
    assert_eq!(value.snapshot().unwrap().value(), &json!("2"));
    assert!(matches!(
        events.borrow().last().unwrap(),
        StateEvent::Reset(_)
    ));
}
#[test]
fn private_candidate_abort_removes_only_reserved_ids_and_preserves_concurrent_activation() {
    let graph = graph();
    let installed = cell(&graph, "installed", json!(1));
    graph.activate().unwrap();
    let (plan, pass) = plan(&graph);
    let candidate = plan
        .declare_cell(
            "candidate".into(),
            StateDeclaration::new(json!({}), json!(3)),
        )
        .unwrap();
    assert_eq!(candidate.snapshot().unwrap().value(), &json!(3));
    pass.set(false);
    assert_eq!(
        candidate.snapshot().unwrap_err().reason,
        "declaration_scope"
    );
    let concurrent = cell(&graph, "concurrent", json!(7));
    plan.abort().finish();
    assert!(candidate.snapshot().is_err());
    assert_eq!(concurrent.snapshot().unwrap().value(), &json!(7));
    assert_eq!(installed.snapshot().unwrap().value(), &json!(1));
}
#[test]
fn initializer_context_reads_completed_candidate_siblings_and_expires_with_real_scope() {
    let graph = graph();
    let (plan, pass) = plan(&graph);
    let ready = plan
        .declare_cell("ready".into(), StateDeclaration::new(json!({}), json!(3)))
        .unwrap();
    let pending = plan
        .declare_cell(
            "pending".into(),
            StateDeclaration::with_initializer(json!({}), |_| async { Ok(json!(9)) }.boxed_local()),
        )
        .unwrap();
    pass.set(false);
    let sources = CandidateSources::new(vec![plan]);
    let (context, scope) = Context::new_root(1, 1, 1, Value::Null);
    let context = sources.authorize_initializer(&context, &sources.plans[0]);
    assert_eq!(ready.snapshot_in(&context).unwrap().value(), &json!(3));
    assert_eq!(
        pending.snapshot_in(&context).unwrap_err().code,
        ErrorCode::NotReady
    );
    assert!(ready.snapshot().is_err());
    drop(scope);
    assert!(ready.snapshot_in(&context).is_err());
}
#[test]
fn cross_object_derived_candidates_see_prospective_values_without_partial_publication() {
    let a = graph();
    let source = cell(&a, "source", json!(2));
    a.activate().unwrap();
    let b = graph();
    let (pa, _) = plan(&a);
    pa.declare_cell(
        "source".into(),
        StateDeclaration::new(json!({"type":"integer"}), json!(999))
            .version(3)
            .with_migrate(|_, _| Ok(json!(3))),
    )
    .unwrap();
    let (pb, _) = plan(&b);
    let derived = pb
        .derive(
            vec![source.readable()],
            json!({"type":"integer"}),
            Rc::new(|values| Ok(json!(values[0].value().as_i64().unwrap() * 10))),
        )
        .unwrap();
    let sources = CandidateSources::new(vec![pa, pb]);
    let mut prepared = sources.prepare_all().unwrap();
    assert_eq!(source.version(), 1);
    assert_eq!(
        source
            .readable
            .port
            .upgrade()
            .unwrap()
            .current()
            .unwrap()
            .value(),
        &json!(2)
    );
    for plan in &prepared {
        plan.preflight().unwrap();
    }
    let ha = a.queue.hold();
    let hb = b.queue.hold();
    for candidate in &mut prepared {
        let batch = candidate.adopt();
        candidate.plan.graph.queue.append_batch(batch);
    }
    for candidate in prepared {
        candidate.finish();
    }
    drop(ha);
    drop(hb);
    assert_eq!(source.snapshot().unwrap().value(), &json!(3));
    assert_eq!(derived.snapshot().unwrap().value(), &json!(30));
}

#[test]
fn ready_barrier_refuses_only_captured_local_writes_before_callbacks_and_releases_on_abort() {
    let provider = graph();
    let observed = cell(&provider, "observed", json!(2));
    let unrelated = cell(&provider, "unrelated", json!(9));
    provider.activate().unwrap();
    let consumer = graph();
    let (plan, _) = plan(&consumer);
    plan.derive(
        vec![observed.readable()],
        json!({"type":"integer"}),
        Rc::new(|values| Ok(values[0].value().clone())),
    )
    .unwrap();
    let sources = CandidateSources::new(vec![plan]);
    let barrier = sources.ready_barrier().unwrap();
    let mut prepared = sources.prepare_all().unwrap();
    for candidate in &mut prepared {
        candidate.release_preparing();
    }
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    let callback = Rc::new(Flag::new(false));
    let ran = callback.clone();
    let error = observed
        .change(&ctx, move |_| {
            ran.set(true);
            Ok(())
        })
        .err()
        .unwrap();
    assert_eq!(error.reason, "upgrade_conflict");
    assert_eq!(error.code, ErrorCode::UpgradeConflict);
    assert!(error.retryable());
    assert!(!callback.get());
    assert_eq!(observed.snapshot().unwrap().value(), &json!(2));
    unrelated
        .replace(
            &ctx,
            json!(10),
            super::super::super::ReplaceOptions::default(),
        )
        .unwrap();
    for candidate in &prepared {
        candidate.preflight().unwrap();
    }
    drop(prepared);
    drop(barrier);
    observed
        .replace(
            &ctx,
            json!(3),
            super::super::super::ReplaceOptions::default(),
        )
        .unwrap();
    assert_eq!(observed.snapshot().unwrap().value(), &json!(3));
}
#[test]
fn initializer_reads_do_not_run_migration_before_gated_projection() {
    let graph = graph();
    let source = cell(&graph, "source", json!(2));
    graph.activate().unwrap();
    let (plan, _) = plan(&graph);
    let migrations = Rc::new(Flag::new(0));
    let count = migrations.clone();
    plan.declare_cell(
        "source".into(),
        StateDeclaration::new(json!({"type":"integer"}), json!(999))
            .version(3)
            .with_migrate(move |old, _| {
                count.set(count.get() + 1);
                assert_eq!(old, json!(2));
                Ok(json!(3))
            }),
    )
    .unwrap();
    let sources = CandidateSources::new(vec![plan.clone()]);
    let (context, _scope) = Context::new_root(1, 2, 1, Value::Null);
    let context = sources.authorize_initializer(&context, &plan);
    assert_eq!(source.snapshot_in(&context).unwrap().value(), &json!(2));
    assert_eq!(migrations.get(), 0);
    sources.prepare_migrations().unwrap();
    assert_eq!(source.snapshot_in(&context).unwrap().value(), &json!(3));
    assert_eq!(source.snapshot().unwrap().value(), &json!(2));
    let prepared = sources.prepare_all().unwrap();
    assert_eq!(migrations.get(), 1);
    drop(prepared);
}

#[test]
fn ready_external_basis_is_frozen_then_reconciles_latest_after_commit() {
    let source_queue = PublicationQueue::new();
    let external = ReadPort::new(
        Rc::new(|| Ok(())),
        Rc::new(|| Ok(())),
        Rc::new(|error| panic!("unexpected source error: {error}")),
        source_queue.clone(),
    );
    let publish = |value, revision| {
        let snapshot = Snapshot::new(Rc::new(value), revision).unwrap();
        let event = if revision == 1 {
            StateEvent::Snapshot(snapshot)
        } else {
            StateEvent::Reset(snapshot)
        };
        source_queue.append_batch(external.adopt(event).into_iter().collect());
    };
    publish(json!(2), 1);
    let graph = graph();
    let (plan, _) = plan(&graph);
    let projected = plan
        .derive(
            vec![external.readable()],
            json!({"type":"integer"}),
            Rc::new(|values| Ok(json!(values[0].value().as_i64().unwrap() * 10))),
        )
        .unwrap();
    let sources = CandidateSources::new(vec![plan]);
    let barrier = sources.ready_barrier().unwrap();
    let mut prepared = sources.prepare_all().unwrap().pop().unwrap();
    prepared.release_preparing();
    publish(json!(3), 2);
    assert_eq!(external.current().unwrap().value(), &json!(3));
    prepared.preflight().unwrap();
    let hold = graph.queue.hold();
    graph.queue.append_batch(prepared.adopt());
    assert_eq!(projected.snapshot().unwrap().value(), &json!(20));
    prepared.finish();
    assert_eq!(projected.snapshot().unwrap().value(), &json!(20));
    drop(barrier);
    assert_eq!(projected.snapshot().unwrap().value(), &json!(30));
    drop(hold);
    publish(json!(4), 3);
    assert_eq!(projected.snapshot().unwrap().value(), &json!(40));
}

#[test]
fn terminated_graph_refuses_prepared_adoption_before_metadata_changes() {
    let graph = graph();
    let value = cell(&graph, "value", json!(2));
    graph.activate().unwrap();
    let (plan, _) = plan(&graph);
    plan.declare_cell(
        "value".into(),
        StateDeclaration::new(json!({"type":"integer"}), json!(0)).version(2),
    )
    .unwrap();
    let sources = CandidateSources::new(vec![plan.clone()]);
    let mut prepared = sources.prepare_all().unwrap().pop().unwrap();
    prepared.release_preparing();
    graph.terminate(RuntimeError::unavailable("closed", "graph ended"));
    assert_eq!(prepared.preflight().unwrap_err().reason, "closed");
    assert_eq!(value.version(), 1);
    drop(prepared);
    plan.abort().finish();
}

#[test]
fn retirement_waits_for_ready_decision_and_noop_retirement_does_not_stale_vote() {
    let graph = graph();
    let kept = cell(&graph, "kept", json!(2));
    let retired = cell(&graph, "retired", json!(3));
    graph.activate().unwrap();
    let old = graph.declarations();
    let (plan, _) = plan(&graph);
    plan.declare_cell(
        "kept".into(),
        StateDeclaration::new(json!({"type":"integer"}), json!(999)),
    )
    .unwrap();
    let sources = CandidateSources::new(vec![plan.clone()]);
    let barrier = sources.ready_barrier().unwrap();
    let mut prepared = sources.prepare_all().unwrap().pop().unwrap();
    prepared.release_preparing();
    let selected = plan.declarations();
    graph
        .retire(&selected, std::slice::from_ref(&selected))
        .finish();
    prepared.preflight().unwrap();
    graph.retire(&old, std::slice::from_ref(&selected)).finish();
    assert_eq!(retired.snapshot().unwrap().value(), &json!(3));
    prepared.preflight().unwrap();
    graph.queue.append_batch(prepared.adopt());
    prepared.finish();
    assert_eq!(retired.snapshot().unwrap().value(), &json!(3));
    drop(barrier);
    assert!(retired.snapshot().is_err());
    assert_eq!(kept.snapshot().unwrap().value(), &json!(2));
}

#[test]
fn initializer_child_authority_is_inherited_but_independent_roots_read_installed_state() {
    let graph = graph();
    let source = cell(&graph, "delegated", json!(2));
    graph.activate().unwrap();
    let (plan, pass) = plan(&graph);
    plan.declare_cell(
        "delegated".into(),
        StateDeclaration::new(json!({"type":"integer"}), json!(0))
            .version(2)
            .with_migrate(|_, _| Ok(json!(3))),
    )
    .unwrap();
    pass.set(false);
    let sources = CandidateSources::new(vec![plan.clone()]);
    sources.prepare_migrations().unwrap();
    let driver = crate::ManualDriver::new();
    let host = crate::Host::new(
        crate::HostOptions::new("initializer-delegation", Value::Null),
        driver.clone(),
    );
    let (cell, owner) = (source.clone(), host.clone());
    let (child, child_guard) = driver
        .run_until(host.root(move |context| {
            async move {
                let authorized = sources.authorize_initializer(&context, &plan);
                let (child, guard) = authorized.child_call(Value::Null)?;
                assert_eq!(cell.snapshot_in(&child)?.value(), &json!(3));
                let independent = cell.clone();
                assert_eq!(
                    owner
                        .root(move |independent_context| async move {
                            Ok(independent
                                .snapshot_in(&independent_context)?
                                .value()
                                .clone())
                        }
                        .boxed_local())
                        .await?,
                    json!(2)
                );
                assert_eq!(cell.snapshot_in(&child)?.value(), &json!(3));
                Ok((child, guard))
            }
            .boxed_local()
        }))
        .unwrap();
    // The returned child guard remains alive, but its actual ancestor root has settled.
    assert_eq!(source.snapshot_in(&child).unwrap_err().reason, "expired");
    assert_eq!(source.snapshot().unwrap().value(), &json!(2));
    drop(child_guard);
    driver.run_until(host.close()).unwrap();
}

fn projection_retirement_case(old_rejects: bool) {
    let a = graph();
    let source = cell(&a, "source", json!(2));
    a.activate().unwrap();
    let b = graph();
    let dropped = Rc::new(Flag::new(0));
    struct Capture(Rc<Flag<u32>>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let capture = Rc::new(Capture(dropped.clone()));
    let weak_capture = Rc::downgrade(&capture);
    let old = b
        .derive(
            vec![source.readable()],
            json!({"type":"integer"}),
            Rc::new(move |values| {
                let _retained = &capture;
                if old_rejects && values[0].value() == &json!(3) {
                    return Err(RuntimeError::invalid(
                        "retired_compute",
                        "old declaration rejects",
                    ));
                }
                Ok(json!(values[0].value().as_i64().unwrap() * 2))
            }),
        )
        .unwrap();
    b.activate().unwrap();
    let old_declarations = b.declarations();
    let (current, _) = plan(&b);
    let current_value = current
        .derive(
            vec![source.readable()],
            json!({"type":"integer"}),
            Rc::new(|values| Ok(json!(values[0].value().as_i64().unwrap() * 10))),
        )
        .unwrap();
    let current_sources = CandidateSources::new(vec![current.clone()]);
    let mut prepared = current_sources.prepare_all().unwrap().pop().unwrap();
    prepared.preflight().unwrap();
    b.queue.append_batch(prepared.adopt());
    prepared.finish();
    let current_declarations = current.declarations();
    drop(current_sources);
    drop(current);
    assert!(weak_capture.upgrade().is_some());
    let (migration, _) = plan(&a);
    migration
        .declare_cell(
            "source".into(),
            StateDeclaration::new(json!({"type":"integer"}), json!(999))
                .version(2)
                .with_migrate(|_, _| Ok(json!(3))),
        )
        .unwrap();
    let sources = CandidateSources::new(vec![migration])
        .with_dependents(vec![current_declarations.clone()], vec![])
        .unwrap();
    let barrier = sources.ready_barrier().unwrap();
    let mut prepared = sources.prepare_all().unwrap();
    for candidate in &mut prepared {
        candidate.release_preparing();
    }
    b.retire(
        &old_declarations,
        std::slice::from_ref(&current_declarations),
    )
    .finish();
    assert!(
        old.snapshot().is_ok(),
        "physical retirement is deferred through Ready"
    );
    let ha = a.queue.hold();
    let hb = b.queue.hold();
    for candidate in &mut prepared {
        candidate.preflight().unwrap();
        let queue = candidate.plan.graph.queue.clone();
        queue.append_batch(candidate.adopt());
    }
    for candidate in prepared {
        candidate.finish();
    }
    drop(barrier);
    drop(ha);
    drop(hb);
    assert!(
        old.snapshot().is_err(),
        "completed retirement must remove old nodes"
    );
    assert!(
        old.port.upgrade().is_none(),
        "no old ReadPort remains owned"
    );
    assert!(
        weak_capture.upgrade().is_none(),
        "old compute captures must be released"
    );
    assert_eq!(dropped.get(), 1);
    assert_eq!(current_value.snapshot().unwrap().value(), &json!(30));
    let (context, _scope) = Context::new_root(1, 99, 2, Value::Null);
    source
        .replace(
            &context,
            json!(4),
            super::super::super::ReplaceOptions::default(),
        )
        .unwrap();
    assert_eq!(current_value.snapshot().unwrap().value(), &json!(40));
}

#[test]
fn dependent_projection_does_not_keep_deferred_retired_declarations_or_callback_captures() {
    projection_retirement_case(false);
}
#[test]
fn retired_derivation_cannot_poison_current_dependent_projection() {
    projection_retirement_case(true);
}
