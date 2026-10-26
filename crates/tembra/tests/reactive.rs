use futures::{FutureExt, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, Orb, Service};

#[test]
fn selections_cache_equal_values_and_reference_release_detaches_observers() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![Orb::new("state", 1).service(
        Service::new("state", |self_| self_.state(0_i32, 1, |_| Ok(()))).method(
            "set",
            |state, ctx, value: i32| async move {
                state.update(&ctx, |_| Ok(value))?;
                Ok(())
            },
        ),
    )])
    .unwrap();
    let reference = Rc::new(host.get("state", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let selected = reference
        .state::<i32>()
        .unwrap()
        .select(|value| Ok(value % 2));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    let observation = selected
        .subscribe(move |value| {
            captured.borrow_mut().push(*value);
            Ok(())
        })
        .unwrap();
    let first = selected.value().unwrap();
    for value in [2, 3, 5, 6] {
        pool.run_until(host.root("alice", {
            let reference = reference.clone();
            move |ctx| {
                async move { reference.call::<_, ()>(&ctx, "set", value).await }.boxed_local()
            }
        }))
        .unwrap();
        if value == 2 {
            assert!(Rc::ptr_eq(&first, &selected.value().unwrap()));
        }
    }
    assert_eq!(*seen.borrow(), [0, 1, 0]);
    reference.release().unwrap();
    assert!(selected.value().is_err());
    observation.detach();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn observer_errors_never_revert_adopted_state() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![Orb::new("state", 1).service(
        Service::new("state", |self_| self_.state(0_i32, 1, |_| Ok(()))).method(
            "set",
            |state, ctx, value: i32| async move {
                state.update(&ctx, |_| Ok(value))?;
                Ok(())
            },
        ),
    )])
    .unwrap();
    let reference = Rc::new(host.get("state", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let state = reference.state::<i32>().unwrap();
    let _observation = state
        .subscribe(|value| {
            if *value == 0 {
                Ok(())
            } else {
                Err(tembra::Error::invalid("observer", "failed"))
            }
        })
        .unwrap();
    pool.run_until(host.root("alice", {
        let reference = reference.clone();
        move |ctx| async move { reference.call::<_, ()>(&ctx, "set", 7_i32).await }.boxed_local()
    }))
    .unwrap();
    assert_eq!(*state.value().unwrap(), 7);
    assert_eq!(host.errors()[0].reason, "observer");
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
