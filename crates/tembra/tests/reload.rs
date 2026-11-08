//! Native callback environments follow the kernel's pinned code versions.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use std::rc::Rc;
use tembra::{Host, Orb, Service};

/// Build two implementations with the same contract and state schema.
fn counter(offset: u32) -> Orb {
    Orb::new("counter", 1).service(
        Service::new("counter", |self_| self_.state(0_u32, 1, |_| Ok(())))
            .method("read", move |state, _, (): ()| async move {
                Ok(*state.value()? + offset)
            })
            .method("set", |state, context, value: u32| async move {
                state.update(&context, |_| Ok(value))?;
                Ok(())
            }),
    )
}

/// Preserve the old callback while sharing compatible state with newly admitted roots.
#[test]
fn old_roots_keep_their_native_implementation() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![counter(1)]).unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let (started, start) = oneshot::channel();
    let (resume, wait) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let old_host = host.clone();
    let old_reference = reference.clone();
    pool.spawner()
        .spawn_local(async move {
            let result = old_host
                .root("alice", move |context| {
                    async move {
                        assert_eq!(old_reference.call::<_, u32>(&context, "read", ()).await?, 1);
                        let _ = started.send(());
                        wait.await.unwrap();
                        old_reference.call::<_, u32>(&context, "read", ()).await
                    }
                    .boxed_local()
                })
                .await;
            let _ = finished.send(result);
        })
        .unwrap();
    pool.run_until(start).unwrap();
    pool.run_until(host.reload(vec![counter(2)], 5000)).unwrap();
    let new_reference = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(new_reference.call::<_, u32>(&context, "read", ()).await?, 2);
            new_reference.call::<_, ()>(&context, "set", 5_u32).await?;
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(pool.run_until(finish).unwrap().unwrap(), 6);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// Refuse a candidate's validator without replacing the current environment or snapshot.
#[test]
fn failed_native_validation_restores_the_current_version() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![counter(1)]).unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let bad = Orb::new("counter", 1).service(
        Service::new("counter", |self_| {
            self_.state(0_u32, 1, |_| {
                Err(tembra::Error::invalid("validation", "candidate refused"))
            })
        })
        .method("read", |_, _, (): ()| async { Ok(99_u32) })
        .method("set", |_, _, _: u32| async { Ok(()) }),
    );
    assert!(pool.run_until(host.reload(vec![bad], 5000)).is_err());
    let called = reference.clone();
    assert_eq!(
        pool.run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        1
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// Reject adding state before candidate capture can initialize the installed object.
#[test]
fn aborted_replacement_cannot_add_state_to_a_live_object() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![Orb::new("counter", 1).service(
        Service::new("counter", |_| Ok(())).method("read", |_, _, (): ()| async { Ok(7_u32) }),
    )])
    .unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    assert!(reference.state::<u32>().is_err());
    let candidate = Orb::new("counter", 1).service(
        Service::new("counter", |self_| self_.state(99_u32, 1, |_| Ok(()))).method(
            "read",
            |state, _, (): ()| async move { Ok(*state.value()?) },
        ),
    );
    let error = pool
        .run_until(host.reload(vec![candidate], 5000))
        .unwrap_err();
    assert_eq!(error.reason, "state_migration");
    assert!(reference.state::<u32>().is_err());
    let called = reference.clone();
    assert_eq!(
        pool.run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        7
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// Refuse unsupported task restart before stopping or changing the installed task.
#[test]
fn active_one_shot_tasks_are_preserved_when_replacement_is_refused() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let stopped = Rc::new(std::cell::Cell::new(false));
    let completed = stopped.clone();
    let orb = Orb::new("counter", 1)
        .create(move |self_| {
            let completed = completed.clone();
            self_.task(move |scope| {
                async move {
                    scope.signal().cancelled().await;
                    completed.set(true);
                    Ok(())
                }
                .boxed_local()
            })?;
            Ok(())
        })
        .service(
            Service::new("counter", |_| Ok(())).method("read", |_, _, (): ()| async { Ok(7_u32) }),
        );
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let error = pool
        .run_until(host.reload(vec![counter(2)], 5000))
        .unwrap_err();
    assert_eq!(error.reason, "task_restart");
    assert!(!stopped.get());
    let called = reference.clone();
    assert_eq!(
        pool.run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        7
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(stopped.get());
}

/// A replacement cannot capture an object whose original deferred factory has not settled.
#[test]
fn pending_activation_refuses_replacement_before_candidate_capture() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let initial = Orb::new("counter", 1).service(
        Service::new("counter", |_| Ok(()))
            .before_create(|| async {
                futures_timer::Delay::new(std::time::Duration::from_millis(20)).await;
                Ok(())
            })
            .method("read", |_, _, (): ()| async { Ok(1_u32) }),
    );
    host.install(vec![initial]).unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "alice").unwrap());
    let error = pool
        .run_until(host.reload(vec![counter(2)], 5000))
        .unwrap_err();
    assert_eq!(error.reason, "activation_pending");
    pool.run_until(reference.ready()).unwrap();
    let candidate = Orb::new("counter", 1).service(
        Service::new("counter", |_| Ok(())).method("read", |_, _, (): ()| async { Ok(2_u32) }),
    );
    pool.run_until(host.reload(vec![candidate], 5000)).unwrap();
    let called = reference.clone();
    assert_eq!(
        pool.run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        2
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
