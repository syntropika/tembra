use super::*;
use crate::{Driver, ManualDriver};
use futures::{FutureExt, channel::oneshot};
use std::cell::Cell;

fn installed(
    driver: &ManualDriver,
    declaration: ResourceDeclaration<u32>,
) -> (ResourceSet, Resource<u32>) {
    let resource = ResourceRecord::declare("value".into(), declaration, Weak::new());
    let value = driver
        .run_until((resource.record.spec().open)(CancellationToken::new()))
        .unwrap();
    *resource.record.value.borrow_mut() = Some(value);
    let set = ResourceSet::new(
        Weak::new(),
        vec![resource.record.clone()],
        vec![resource.record.clone()],
    );
    (set, resource)
}
#[test]
fn kept_value_adopts_new_closer_and_public_handle_does_not_extend_managed_lifetime() {
    let driver = ManualDriver::new();
    let old_closes = Rc::new(Cell::new(0));
    let new_closes = Rc::new(Cell::new(0));
    let closed = old_closes.clone();
    let (set, old) = installed(
        &driver,
        ResourceDeclaration::new(|_| async { Ok(7) }.boxed_local()).with_close(move |_| {
            closed.set(closed.get() + 1);
            async { Ok(()) }.boxed_local()
        }),
    );
    let identity = old.value().unwrap();
    let plan = ResourceReload::new(&set);
    let closed = new_closes.clone();
    let kept = plan
        .declare(
            "value".into(),
            ResourceDeclaration::new(|_| {
                async { panic!("kept opener must not run") }.boxed_local()
            })
            .with_close(move |value: Rc<u32>| {
                assert_eq!(*value, 7);
                closed.set(closed.get() + 1);
                async { Ok(()) }.boxed_local()
            }),
        )
        .unwrap();
    driver
        .run_until(plan.open_nonexclusive(CancellationToken::new()))
        .unwrap();
    plan.preflight().unwrap();
    let (next, retired) = plan.adopt();
    retired.finish();
    assert!(Rc::ptr_eq(&identity, &kept.value().unwrap()));
    driver
        .run_until(set.retire(std::slice::from_ref(&next)))
        .unwrap();
    assert_eq!(old_closes.get(), 0);
    driver.run_until(next.retire(&[])).unwrap();
    assert_eq!(new_closes.get(), 1);
    assert!(old.value().is_err());
    assert!(kept.value().is_err());
    assert_eq!(*identity, 7);
}
#[test]
fn replacement_keeps_original_closer_while_candidate_closes_independently() {
    let driver = ManualDriver::new();
    let log = Rc::new(RefCell::new(Vec::new()));
    let closed = log.clone();
    let (set, old) = installed(
        &driver,
        ResourceDeclaration::new(|_| async { Ok(1) }.boxed_local()).with_close(move |value| {
            closed.borrow_mut().push(*value);
            async { Ok(()) }.boxed_local()
        }),
    );
    let plan = ResourceReload::new(&set);
    let closed = log.clone();
    let mut declaration = ResourceDeclaration::new(|_| async { Ok(2_u32) }.boxed_local())
        .with_close(move |value| {
            closed.borrow_mut().push(*value);
            async { Ok(()) }.boxed_local()
        });
    declaration.version = 2;
    let new = plan.declare("value".into(), declaration).unwrap();
    driver
        .run_until(plan.open_nonexclusive(CancellationToken::new()))
        .unwrap();
    plan.preflight().unwrap();
    let (next, retired) = plan.adopt();
    retired.finish();
    driver
        .run_until(set.retire(std::slice::from_ref(&next)))
        .unwrap();
    assert_eq!(&*log.borrow(), &[1]);
    assert!(old.value().is_err());
    assert_eq!(*new.value().unwrap(), 2);
    driver.run_until(next.retire(&[])).unwrap();
    assert_eq!(&*log.borrow(), &[1, 2]);
}
#[test]
fn adoption_check_refuses_before_installed_mutation_or_candidate_open() {
    let driver = ManualDriver::new();
    let (set, old) = installed(
        &driver,
        ResourceDeclaration::new(|_| async { Ok(9) }.boxed_local()),
    );
    let identity = old.value().unwrap();
    let plan = ResourceReload::new(&set);
    plan.declare(
        "value".into(),
        ResourceDeclaration::new(|_| async { panic!("not reopened") }.boxed_local())
            .with_adoption_check(|_: &u32| {
                Err(RuntimeError::invalid(
                    "resource_restart",
                    "incompatible private value",
                ))
            }),
    )
    .unwrap();
    assert_eq!(
        plan.preflight_adoption().unwrap_err().reason,
        "resource_restart"
    );
    assert!(Rc::ptr_eq(&identity, &old.value().unwrap()));
    assert_eq!(old.version(), 1);
    driver
        .run_until(plan.abort(RuntimeError::aborted("prepare", "candidate refused")))
        .unwrap();
    assert!(Rc::ptr_eq(&identity, &old.value().unwrap()));
}
#[test]
fn failed_exclusive_close_is_journaled_before_callback_and_restored() {
    let driver = ManualDriver::new();
    let opens = Rc::new(Cell::new(0));
    let closing = Rc::new(Cell::new(0));
    let opened = opens.clone();
    let closed = closing.clone();
    let mut declaration = ResourceDeclaration::new(move |_| {
        opened.set(opened.get() + 1);
        async { Ok(4) }.boxed_local()
    })
    .with_close(move |_| {
        closed.set(closed.get() + 1);
        async {
            Err(RuntimeError::invalid(
                "external_close",
                "effect already happened",
            ))
        }
        .boxed_local()
    });
    declaration.exclusive = true;
    let (set, old) = installed(&driver, declaration);
    let plan = ResourceReload::new(&set);
    assert!(plan.requires_gate());
    let cause = driver
        .run_until(plan.replace_exclusive(CancellationToken::new()))
        .unwrap_err();
    assert_eq!(cause.reason, "external_close");
    assert!(old.value().is_err());
    assert_eq!(closing.get(), 1);
    driver.run_until(plan.abort(cause)).unwrap();
    assert_eq!(opens.get(), 2);
    assert_eq!(*old.value().unwrap(), 4);
}
#[test]
fn restoration_failure_marks_only_that_record_unavailable_with_structured_original_cause() {
    let driver = ManualDriver::new();
    let opens = Rc::new(Cell::new(0));
    let opened = opens.clone();
    let mut declaration = ResourceDeclaration::new(move |_| {
        opened.set(opened.get() + 1);
        let count = opened.get();
        async move {
            if count == 1 {
                Ok(4)
            } else {
                Err(RuntimeError::invalid("restore_open", "cannot restore"))
            }
        }
        .boxed_local()
    });
    declaration.exclusive = true;
    let (set, old) = installed(&driver, declaration);
    let plan = ResourceReload::new(&set);
    driver
        .run_until(plan.replace_exclusive(CancellationToken::new()))
        .unwrap();
    let error = driver
        .run_until(plan.abort(RuntimeError::invalid("candidate_open", "candidate failed")))
        .unwrap_err();
    assert_eq!(error.reason, "resource_cleanup");
    assert_eq!(error.details()["reload"]["reason"], "candidate_open");
    let unavailable = old.value().unwrap_err();
    assert_eq!(unavailable.reason, "resource_restore");
    assert_eq!(unavailable.details()["cause"]["reason"], "restore_open");
}
#[test]
fn abort_waits_for_late_success_and_closes_it_once() {
    let driver = ManualDriver::new();
    let plan = ResourceReload::new(&ResourceSet::new(Weak::new(), vec![], vec![]));
    let (tx, rx) = oneshot::channel::<u32>();
    let rx = Rc::new(RefCell::new(Some(rx)));
    let closed = Rc::new(Cell::new(0));
    let count = closed.clone();
    plan.declare(
        "late".into(),
        ResourceDeclaration::new(move |_| {
            let rx = rx.borrow_mut().take().unwrap();
            async move { Ok(rx.await.unwrap()) }.boxed_local()
        })
        .with_close(move |_: Rc<u32>| {
            count.set(count.get() + 1);
            async { Ok(()) }.boxed_local()
        }),
    )
    .unwrap();
    let opening = plan.clone();
    driver.spawn(
        async move {
            let _ = opening.open_nonexclusive(CancellationToken::new()).await;
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    let finished = Rc::new(Cell::new(false));
    let done = finished.clone();
    let abort = plan.clone();
    driver.spawn(
        async move {
            abort
                .abort(RuntimeError::aborted("cancel", "stop candidate"))
                .await
                .unwrap();
            done.set(true);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(!finished.get());
    assert_eq!(closed.get(), 0);
    tx.send(7).unwrap();
    driver.run_until_stalled();
    assert!(finished.get());
    assert_eq!(closed.get(), 1);
}
