use super::*;

#[test]
fn identities_cross_u32_and_stop_at_the_javascript_safe_integer_limit() {
    let host = Host::new();
    host.inner.borrow_mut().next = u32::MAX.into();
    assert_eq!(host.allocate_handle().unwrap(), u64::from(u32::MAX));
    assert_eq!(host.allocate_handle().unwrap(), u64::from(u32::MAX) + 1);
    host.inner.borrow_mut().next = 9_007_199_254_740_990;
    assert_eq!(host.allocate_handle().unwrap(), 9_007_199_254_740_990);
    assert_eq!(host.allocate_handle().unwrap(), 9_007_199_254_740_991);
    let next = host.inner.borrow().next;
    for _ in 0..2 {
        let error = host.allocate_handle().unwrap_err();
        assert_eq!(error.code, Code::Exhausted);
        assert_eq!(error.reason, Reason::Identities.as_str());
        assert_eq!(host.inner.borrow().next, next);
    }
}

#[test]
fn multi_identity_reservation_at_the_limit_leaves_preparation_unchanged() {
    let host = Host::new();
    host.inner.borrow_mut().next = MAX_ID;
    let before = host.diagnostics();
    let error = host
        .prepare(vec![Orb::new("limit", 1)], vec![])
        .unwrap_err();
    assert_eq!(error.code, Code::Exhausted);
    assert_eq!(host.diagnostics(), before);
    assert!(host.inner.borrow().pending.is_none());
    assert_eq!(host.allocate_handle().unwrap(), MAX_ID);
}

fn install(host: &Host) -> InstalledOrb {
    let reload = host.prepare(vec![Orb::new("example", 1)], vec![]).unwrap();
    let orb = reload.orbs[0].clone();
    host.commit(reload.id, vec![]).unwrap();
    orb
}

#[test]
fn admitted_roots_retain_old_versions_but_new_task_roots_use_current_code() {
    let host = Host::new();
    let old = install(&host);
    let root = host.begin_root("host").unwrap();
    let task = host.begin_task(Owner::Orb(old.owner)).unwrap();
    let current = install(&host);
    for _ in 0..100 {
        let root = host.begin_root("host").unwrap();
        host.end_root(root).unwrap();
    }
    assert_eq!(host.inner.borrow().versions.len(), 2);
    assert!(host.orb(old.id).is_ok());
    let retained = host.task_root(task, "host").unwrap();
    assert_eq!(
        host.inner.borrow().roots[&retained].version,
        current.code_version
    );
    host.end_root(root).unwrap();
    assert!(host.orb(old.id).is_err());
    host.finish_task(task).unwrap();
    host.end_root(retained).unwrap();
    assert_eq!(host.inner.borrow().versions.len(), 1);
    assert!(host.orb(old.id).is_err());
    assert!(host.orb(current.id).is_ok());
    assert!(host.inner.borrow().version_users.is_empty());
    assert!(host.drain().contains(&Action::RetireOrb(old.id)));
}

#[test]
fn an_unrelated_task_does_not_exhaust_the_retained_version_limit() {
    let host = Host::new();
    let owner = install(&host).owner;
    let task = host.begin_task(Owner::Orb(owner)).unwrap();
    for _ in 0..40 {
        let orb = install(&host);
        let root = host.task_root(task, "host").unwrap();
        assert_eq!(host.inner.borrow().roots[&root].version, orb.code_version);
        host.end_root(root).unwrap();
        assert_eq!(host.inner.borrow().versions.len(), 1);
    }
    host.finish_task(task).unwrap();
}

fn lingering_object(host: &Host, linger_ms: u32) -> Acquisition {
    let mut orb = Orb::new("linger", 1);
    let mut service = Service::new("linger", ["read"]);
    service.linger_ms = linger_ms;
    orb.services.push(service);
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let claim = host
        .acquire(
            Address {
                service: "linger".into(),
                key: vec!["main".into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap();
    host.ready(claim.object).unwrap();
    host.drain();
    claim
}

#[test]
fn linger_keeps_tasks_alive_and_reacquisition_resets_the_idle_deadline() {
    let host = Host::new();
    let claim = lingering_object(&host, 100);
    let address = host.object_address(claim.object).unwrap();
    let task = host.begin_task(Owner::Object(claim.object)).unwrap();
    host.release(claim.reference).unwrap();
    assert!(host.drain().is_empty());
    assert_eq!(host.next_idle(), Some(100));
    let root = host.task_root(task, "host").unwrap();
    host.end_root(root).unwrap();
    host.tick(50).unwrap();
    let again = host.acquire(address.clone(), 1, BTreeSet::new()).unwrap();
    assert_eq!(again.object, claim.object);
    assert!(!again.activate);
    assert_eq!(host.next_idle(), None);
    host.tick(150).unwrap();
    assert!(host.drain().is_empty());
    host.release(again.reference).unwrap();
    host.tick(249).unwrap();
    assert_eq!(host.next_idle(), Some(1));
    assert!(host.drain().is_empty());
    host.tick(250).unwrap();
    assert_eq!(host.drain(), [Action::CancelTask(task)]);
    assert_eq!(
        host.task_root(task, "host").unwrap_err().code,
        Code::Cancelled
    );
    assert_eq!(
        host.acquire(address.clone(), 1, BTreeSet::new())
            .unwrap_err()
            .reason,
        Reason::Disposing.as_str()
    );
    assert_eq!(
        host.disposed(claim.object).unwrap_err().reason,
        Reason::CleanupBusy.as_str()
    );
    host.tick(251).unwrap();
    assert!(host.drain().is_empty());
    host.finish_task(task).unwrap();
    assert_eq!(host.drain(), [Action::DisposeObject(claim.object)]);
    host.disposed(claim.object).unwrap();
    let next = host.acquire(address, 1, BTreeSet::new()).unwrap();
    assert_ne!(next.object, claim.object);
    host.close();
    assert_eq!(host.drain(), [Action::DisposeObject(next.object)]);
    host.disposed(next.object).unwrap();
    host.finish_close().unwrap();
}

#[test]
fn zero_linger_and_shutdown_cancel_tasks_without_waiting_for_a_linger_deadline() {
    for linger_ms in [0, 60_000] {
        let host = Host::new();
        let claim = lingering_object(&host, linger_ms);
        let task = host.begin_task(Owner::Object(claim.object)).unwrap();
        host.release(claim.reference).unwrap();
        if linger_ms == 0 {
            assert_eq!(host.drain(), [Action::CancelTask(task)]);
        } else {
            assert!(host.drain().is_empty());
            host.close();
            assert_eq!(host.drain(), [Action::CancelTask(task)]);
            assert_eq!(host.next_idle(), None);
        }
        host.finish_task(task).unwrap();
        assert_eq!(host.drain(), [Action::DisposeObject(claim.object)]);
        host.disposed(claim.object).unwrap();
        host.close();
        host.finish_close().unwrap();
    }
}

#[test]
fn a_task_finishing_during_linger_preserves_the_original_deadline() {
    let host = Host::new();
    let claim = lingering_object(&host, 100);
    let task = host.begin_task(Owner::Object(claim.object)).unwrap();
    host.release(claim.reference).unwrap();
    assert!(host.drain().is_empty());
    host.tick(40).unwrap();
    host.finish_task(task).unwrap();
    assert!(host.drain().is_empty());
    assert_eq!(host.next_idle(), Some(60));
    host.tick(100).unwrap();
    assert_eq!(host.drain(), [Action::DisposeObject(claim.object)]);
    host.disposed(claim.object).unwrap();
    host.close();
    host.finish_close().unwrap();
}

#[test]
fn shutdown_releases_only_the_dependents_of_each_object() {
    let host = Host::new();
    let mut orb = Orb::new("example", 1);
    orb.services.push(Service::new("example", ["read"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let get = |key: &str| {
        host.acquire(
            Address {
                service: "example".into(),
                key: vec![key.into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap()
    };
    let consumer = get("consumer");
    let dependency = get("dependency");
    host.ready(consumer.object).unwrap();
    host.ready(dependency.object).unwrap();
    host.dependency(consumer.object, dependency.reference)
        .unwrap();
    host.close();
    assert_eq!(host.drain(), [Action::DisposeObject(consumer.object)]);
    host.disposed(consumer.object).unwrap();
    assert_eq!(host.drain(), [Action::DisposeObject(dependency.object)]);
    host.disposed(dependency.object).unwrap();
    host.finish_close().unwrap();
}

#[test]
fn a_forgotten_dependency_cannot_block_provider_shutdown() {
    let host = Host::new();
    let mut orb = Orb::new("example", 1);
    let mut service = Service::new("example", ["read"]);
    service.linger_ms = 100;
    orb.services.push(service);
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let get = |key: &str| {
        host.acquire(
            Address {
                service: "example".into(),
                key: vec![key.into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap()
    };
    let consumer = get("consumer");
    let provider = get("provider");
    host.ready(consumer.object).unwrap();
    host.ready(provider.object).unwrap();
    host.dependency(consumer.object, provider.reference)
        .unwrap();
    let scope = host.allocate_handle().unwrap();
    host.register_scope(crate::Scope {
        id: scope,
        owner: Owner::Host,
        orb: 0,
        parents: vec![],
        dependencies: vec![provider.reference],
        resources: vec![],
        state_version: None,
        migrate: false,
        active: true,
        closing: false,
    })
    .unwrap();
    host.release(provider.reference).unwrap();
    host.forget_reference(provider.reference).unwrap();
    assert!(
        !host
            .inner
            .borrow()
            .object_dependents
            .contains(&provider.object)
    );
    assert!(
        !host
            .inner
            .borrow()
            .scope_dependencies
            .contains(&provider.object)
    );
    host.close();
    let actions = host.drain();
    assert!(actions.contains(&Action::DisposeObject(provider.object)));
    assert!(actions.contains(&Action::DisposeObject(consumer.object)));
}

/// A live provider object with two declared methods and one claim requiring only `read`.
fn narrowed_claim(host: &Host) -> Acquisition {
    let mut orb = Orb::new("dup", 1);
    orb.services.push(Service::new("dup", ["read", "write"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let claim = host
        .acquire(
            Address {
                service: "dup".into(),
                key: vec!["main".into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::from(["read".to_owned()]),
        )
        .unwrap();
    host.ready(claim.object).unwrap();
    host.drain();
    claim
}

#[test]
fn duplicate_is_an_independent_claim_with_the_same_admission_and_no_activation() {
    let host = Host::new();
    let claim = narrowed_claim(&host);
    let objects = host.diagnostics().objects;
    let copy = host.duplicate(claim.reference).unwrap();
    assert_ne!(copy, claim.reference);
    // Same object, nothing activated, resolved or scheduled.
    assert_eq!(host.diagnostics().objects, objects);
    assert!(host.drain().is_empty());
    assert_eq!(
        host.reference_methods(copy).unwrap(),
        host.reference_methods(claim.reference).unwrap()
    );
    // The copy outlives the original and keeps the object live.
    host.release(claim.reference).unwrap();
    host.forget_reference(claim.reference).unwrap();
    assert!(host.drain().is_empty());
    assert_eq!(host.status(copy).unwrap(), Status::Live);
    let root = host.begin_root("host").unwrap();
    let call = host.begin_call(copy, root, "read").unwrap();
    // The required-method narrowing is copied, not re-derived from the service.
    let refused = host.begin_call(copy, root, "write").unwrap_err();
    assert_eq!(refused.code, Code::Forbidden);
    host.finish_call(call.id).unwrap();
    host.end_root(root).unwrap();
    host.release(copy).unwrap();
    assert!(host.drain().contains(&Action::DisposeObject(claim.object)));
}

#[cfg(feature = "views")]
#[test]
fn duplicate_copies_the_view_restriction() {
    let host = Host::new();
    let claim = narrowed_claim(&host);
    host.restrict(claim.reference, BTreeSet::new()).unwrap();
    let copy = host.duplicate(claim.reference).unwrap();
    assert!(host.reference_methods(copy).unwrap().is_empty());
    let root = host.begin_root("host").unwrap();
    let refused = host.begin_call(copy, root, "read").unwrap_err();
    assert_eq!(refused.code, Code::Forbidden);
    host.end_root(root).unwrap();
}

#[test]
fn duplicate_refuses_released_unknown_and_closing() {
    let host = Host::new();
    let claim = narrowed_claim(&host);
    let copy = host.duplicate(claim.reference).unwrap();
    host.release(claim.reference).unwrap();
    assert_eq!(
        host.duplicate(claim.reference).unwrap_err().code,
        Code::Released
    );
    assert_eq!(host.duplicate(9_999).unwrap_err().code, Code::Gone);
    // A closing host disposes live objects and admits no new claim, even from a live one.
    host.close();
    assert!(host.duplicate(copy).is_err());
}

#[test]
fn disposal_waiters_follow_exact_physical_instance_and_unregister_on_drop() {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Wake, Waker},
    };
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let host = Host::new();
    let claim = lingering_object(&host, 0);
    let address = host.object_address(claim.object).unwrap();
    assert!(host.wait_for_disposal(&address).is_none());
    host.release(claim.reference).unwrap();
    let mut first = host.wait_for_disposal(&address).unwrap();
    let mut second = host.wait_for_disposal(&address).unwrap();
    let abandoned = host.wait_for_disposal(&address).unwrap();
    assert_eq!(host.inner.borrow().disposal_waiters[&claim.object].len(), 3);
    drop(abandoned);
    assert_eq!(host.inner.borrow().disposal_waiters[&claim.object].len(), 2);
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let mut context = Context::from_waker(&waker);
    assert!(Pin::new(&mut first).poll(&mut context).is_pending());
    assert!(Pin::new(&mut second).poll(&mut context).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    assert_eq!(host.drain(), [Action::DisposeObject(claim.object)]);
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    host.disposed(claim.object).unwrap();
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
    assert!(host.inner.borrow().disposal_waiters.is_empty());
    assert!(Pin::new(&mut first).poll(&mut context).is_ready());
    assert!(Pin::new(&mut second).poll(&mut context).is_ready());
    assert!(host.wait_for_disposal(&address).is_none());
    drop((first, second));
    host.close();
    host.finish_close().unwrap();
}

#[test]
fn shutdown_wakes_disposal_waiters_before_physical_close_finishes() {
    use std::{future::Future, pin::Pin, task::Context};
    let host = Host::new();
    let claim = lingering_object(&host, 0);
    let address = host.object_address(claim.object).unwrap();
    host.release(claim.reference).unwrap();
    let mut waiting = host.wait_for_disposal(&address).unwrap();
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(Pin::new(&mut waiting).poll(&mut context).is_pending());
    host.close();
    assert!(host.inner.borrow().disposal_waiters.is_empty());
    assert!(Pin::new(&mut waiting).poll(&mut context).is_ready());
    assert_eq!(host.diagnostics().objects, 1);
    host.disposed(claim.object).unwrap();
    host.finish_close().unwrap();
}
