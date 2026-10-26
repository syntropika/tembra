use std::collections::BTreeSet;
use tembra_core::*;

fn orb() -> Orb {
    let mut f = Orb::new("counter", 1);
    f.services.push(Service::new("counter", ["increment"]));
    f
}

fn install(host: &Host, orb: Orb) {
    let r = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(r.id, vec![]).unwrap();
}

fn acquire(host: &Host, key: Vec<&str>) -> Acquisition {
    host.acquire(
        Address {
            service: "counter".into(),
            key: key.into_iter().map(String::from).collect(),
            principal: "host".into(),
        },
        1,
        BTreeSet::new(),
    )
    .unwrap()
}

#[test]
fn identity_claims_and_tuple_boundaries() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["a:b", "c"]);
    let b = acquire(&h, vec!["a", "b:c"]);
    let same = acquire(&h, vec!["a:b", "c"]);
    assert_ne!(a.object, b.object);
    assert_eq!(a.object, same.object);
    assert_ne!(a.reference, same.reference);
    h.ready(a.object).unwrap();
    h.release(a.reference).unwrap();
    h.release(a.reference).unwrap();
    assert_eq!(h.status(a.reference).unwrap(), Status::Released);
    assert_eq!(h.status(same.reference).unwrap(), Status::Live);
}

#[test]
fn physical_call_outlives_release_and_cancellation() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["main"]);
    h.ready(a.object).unwrap();
    let root = h.begin_root("host").unwrap();
    let call = h.begin_call(a.reference, root, "increment").unwrap();
    h.cancel_root(root).unwrap();
    h.end_root(root).unwrap();
    h.release(a.reference).unwrap();
    assert!(
        !h.drain()
            .iter()
            .any(|a| matches!(a, Action::DisposeObject(_)))
    );
    assert_eq!(h.diagnostics().calls, 1);
    h.finish_call(call.id).unwrap();
    assert!(h.drain().contains(&Action::DisposeObject(a.object)));
    assert!(
        h.acquire(
            Address {
                service: "counter".into(),
                key: vec!["main".into()],
                principal: "host".into()
            },
            1,
            BTreeSet::new()
        )
        .is_err()
    );
    h.disposed(a.object).unwrap();
    assert!(acquire(&h, vec!["main"]).activate);
}

#[test]
fn stale_candidate_and_foreign_owner_leave_state_unchanged() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["a"]);
    let b = acquire(&h, vec!["b"]);
    h.initialize_state(a.object, 100, 1).unwrap();
    h.initialize_state(b.object, 200, 1).unwrap();
    h.ready(a.object).unwrap();
    h.ready(b.object).unwrap();
    let root = h.begin_root("host").unwrap();
    let call = h.begin_call(a.reference, root, "increment").unwrap();
    let old = h.prepare_state(call.id, a.object, 0, 101).unwrap();
    h.adopt_state(call.id, a.object, 0, 102).unwrap();
    h.drain();
    assert_eq!(old.adopt().unwrap_err().reason, "state_revision");
    assert_eq!(
        h.adopt_state(call.id, b.object, 0, 201).unwrap_err().code,
        Code::Forbidden
    );
    assert_eq!(h.snapshot(a.object).unwrap().value, 102);
    assert_eq!(h.snapshot(b.object).unwrap().value, 200);
    assert!(h.drain().is_empty());
    h.end_root(root).unwrap();
    assert!(h.adopt_state(call.id, a.object, 1, 103).is_err());
    h.finish_call(call.id).unwrap();
}

#[test]
fn old_roots_refuse_new_and_recreated_objects_without_admitting_calls() {
    for recreate in [false, true] {
        let h = Host::new();
        install(&h, orb());
        let retained = acquire(&h, vec!["retained"]);
        h.ready(retained.object).unwrap();
        let old = h.begin_root("host").unwrap();
        if recreate {
            let previous = acquire(&h, vec!["fresh"]);
            h.ready(previous.object).unwrap();
            h.release(previous.reference).unwrap();
            assert!(h.drain().contains(&Action::DisposeObject(previous.object)));
            h.disposed(previous.object).unwrap();
        }
        install(&h, orb());
        let fresh = acquire(&h, vec!["fresh"]);
        let snapshot = h.initialize_state(fresh.object, 77, 1).unwrap();
        h.ready(fresh.object).unwrap();
        let before = h.diagnostics().calls;
        let error = h.begin_call(fresh.reference, old, "increment").unwrap_err();
        assert_eq!(error.code, Code::Conflict);
        assert_eq!(error.reason, Reason::UpgradeConflict.as_str());
        let error = h
            .begin_operation(retained.orb, old, 1, Some(fresh.object))
            .unwrap_err();
        assert_eq!(error.reason, Reason::UpgradeConflict.as_str());
        assert_eq!(h.diagnostics().calls, before);
        assert_eq!(h.status(fresh.reference).unwrap(), Status::Live);
        assert_eq!(h.snapshot(fresh.object).unwrap(), snapshot);
        let retained_call = h.begin_call(retained.reference, old, "increment").unwrap();
        assert_eq!(retained_call.orb, retained.orb);
        h.finish_call(retained_call.id).unwrap();
        let current = h.begin_root("host").unwrap();
        let call = h.begin_call(fresh.reference, current, "increment").unwrap();
        assert_eq!(call.orb, fresh.orb);
        assert_eq!(call.code_version, fresh.code_version);
        h.finish_call(call.id).unwrap();
        h.end_root(current).unwrap();
        h.end_root(old).unwrap();
    }
}

#[test]
fn compatible_reload_keeps_admitted_code() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["main"]);
    h.ready(a.object).unwrap();
    let old = h.begin_root("host").unwrap();
    let first = h.begin_call(a.reference, old, "increment").unwrap();
    h.finish_call(first.id).unwrap();
    install(&h, orb());
    let old_call = h.begin_call(a.reference, old, "increment").unwrap();
    let new = h.begin_root("host").unwrap();
    let new_call = h.begin_call(a.reference, new, "increment").unwrap();
    assert_ne!(old_call.orb, new_call.orb);
    assert_ne!(old_call.code_version, new_call.code_version);
    h.finish_call(old_call.id).unwrap();
    h.end_root(old).unwrap();
    h.finish_call(new_call.id).unwrap();
    h.end_root(new).unwrap();
    assert_eq!(h.installed_orbs().len(), 1);
}

#[test]
fn gate_waits_for_the_whole_root_and_fences_old_contexts_after_commit() {
    let host = Host::new();
    install(&host, orb());
    let object = acquire(&host, vec!["main"]);
    host.ready(object.object).unwrap();
    let unrelated = acquire(&host, vec!["unrelated"]);
    host.ready(unrelated.object).unwrap();
    let admitted = host.begin_root("host").unwrap();
    let first = host
        .begin_call(object.reference, admitted, "increment")
        .unwrap();
    host.finish_call(first.id).unwrap();
    let late = host.begin_root("host").unwrap();
    let other_call = host
        .begin_call(unrelated.reference, late, "increment")
        .unwrap();
    host.finish_call(other_call.id).unwrap();
    let reload = host.prepare(vec![orb()], vec![]).unwrap();
    assert!(
        !host.gate(reload.id, &[object.object]).unwrap(),
        "a root retains its object lease between calls"
    );
    assert_eq!(
        host.commit(reload.id, vec![]).unwrap_err().reason,
        "gate_busy"
    );
    assert_eq!(
        host.begin_call(object.reference, late, "increment")
            .unwrap_err()
            .reason,
        "upgrade_conflict"
    );
    let reentry = host
        .begin_call(object.reference, admitted, "increment")
        .unwrap();
    host.end_root(admitted).unwrap();
    assert!(
        !host.gate(reload.id, &[object.object]).unwrap(),
        "ending the observer must not release a physical call"
    );
    host.finish_call(reentry.id).unwrap();
    assert!(host.gate(reload.id, &[object.object]).unwrap());
    host.commit(reload.id, vec![]).unwrap();
    assert_eq!(
        host.begin_call(object.reference, late, "increment")
            .unwrap_err()
            .reason,
        "upgrade_conflict"
    );
    let other_call = host
        .begin_call(unrelated.reference, late, "increment")
        .unwrap();
    host.finish_call(other_call.id).unwrap();
    let fresh = host.begin_root("host").unwrap();
    let call = host
        .begin_call(object.reference, fresh, "increment")
        .unwrap();
    assert_eq!(call.code_version, reload.code_version);
    host.finish_call(call.id).unwrap();
    host.end_root(fresh).unwrap();
    host.end_root(late).unwrap();
}

#[test]
fn migration_gates_first_access_and_rejects_old_writes() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["main"]);
    h.initialize_state(a.object, 100, 1).unwrap();
    h.ready(a.object).unwrap();
    let admitted = h.begin_root("host").unwrap();
    let call = h.begin_call(a.reference, admitted, "increment").unwrap();
    let late = h.begin_root("host").unwrap();
    let r = h.prepare(vec![orb()], vec![]).unwrap();
    assert!(!h.gate(r.id, &[a.object]).unwrap());
    assert_eq!(
        h.begin_call(a.reference, late, "increment")
            .unwrap_err()
            .reason,
        "upgrade_conflict"
    );
    let reentry = h.begin_call(a.reference, admitted, "increment").unwrap();
    h.finish_call(reentry.id).unwrap();
    assert_eq!(h.commit(r.id, vec![]).unwrap_err().reason, "gate_busy");
    h.finish_call(call.id).unwrap();
    h.end_root(admitted).unwrap();
    assert!(h.gate(r.id, &[a.object]).unwrap());
    h.commit(
        r.id,
        vec![Migration {
            object: a.object,
            expected_revision: 0,
            value: 101,
            state_version: 2,
        }],
    )
    .unwrap();
    assert_eq!(
        h.begin_call(a.reference, late, "increment")
            .unwrap_err()
            .reason,
        "upgrade_conflict"
    );
    assert_eq!(
        h.snapshot(a.object).unwrap(),
        Snapshot {
            value: 101,
            revision: 1,
            version: 2
        }
    );
}

#[test]
fn abort_restores_admission_and_preparation_isolated() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["main"]);
    h.ready(a.object).unwrap();
    let root = h.begin_root("host").unwrap();
    let r = h.prepare(vec![orb()], vec![]).unwrap();
    assert_eq!(h.installed_orbs()[0].id, a.orb);
    h.gate(r.id, &[a.object]).unwrap();
    h.abort(r.id).unwrap();
    assert!(h.begin_call(a.reference, root, "increment").is_ok());
}

#[test]
fn resource_dependencies_and_host_ownership() {
    let h = Host::new();
    let a = h
        .resource(Owner::Host, "connection".into(), 1, false, vec![])
        .unwrap();
    h.resource_opened(a.id, 100).unwrap();
    let b = h
        .resource(Owner::Host, "watch".into(), 1, false, vec![a.id])
        .unwrap();
    h.resource_opened(b.id, 101).unwrap();
    assert!(!h.close_resource(a.id).unwrap());
    assert!(h.close_resource(b.id).unwrap());
    h.resource_closed(b.id).unwrap();
    assert!(h.close_resource(a.id).unwrap());
    h.resource_closed(a.id).unwrap();
    h.close();
    h.finish_close().unwrap();
    assert!(h.begin_root("host").is_err());
}

#[test]
fn linger_uses_monotonic_time_and_reacquisition_cancels_disposal() {
    let h = Host::new();
    let mut f = orb();
    f.services[0].linger_ms = 20;
    install(&h, f);
    let a = acquire(&h, vec!["main"]);
    h.ready(a.object).unwrap();
    h.release(a.reference).unwrap();
    h.tick(19).unwrap();
    assert!(h.drain().is_empty());
    let b = acquire(&h, vec!["main"]);
    assert_eq!(a.object, b.object);
    h.tick(20).unwrap();
    assert!(h.drain().is_empty());
    h.release(b.reference).unwrap();
    h.tick(40).unwrap();
    assert!(h.drain().contains(&Action::DisposeObject(a.object)));
    assert!(h.tick(39).is_err());
}

#[test]
fn tasks_retain_owner_until_physical_completion() {
    let h = Host::new();
    install(&h, orb());
    let a = acquire(&h, vec!["main"]);
    h.ready(a.object).unwrap();
    let task = h.begin_task(Owner::Object(a.object)).unwrap();
    h.release(a.reference).unwrap();
    let actions = h.drain();
    assert!(actions.contains(&Action::CancelTask(task)));
    assert!(!actions.contains(&Action::DisposeObject(a.object)));
    assert!(h.task_root(task, "host").is_err());
    h.finish_task(task).unwrap();
    assert!(h.drain().contains(&Action::DisposeObject(a.object)));
}
