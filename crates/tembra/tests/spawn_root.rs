//! A spawned root keeps its authority until its own operation ends, not its caller's wait.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Context, Host};

#[test]
fn spawned_roots_outlive_their_dropped_receiver_and_end_with_their_operation() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (release, released) = oneshot::channel::<()>();
    let issued: Rc<RefCell<Option<Context>>> = Rc::default();
    let cell = issued.clone();
    let receiver = host
        .spawn_root("alice", move |context| {
            *cell.borrow_mut() = Some(context.clone());
            async move {
                let _ = released.await;
                // Authority is still checked after the caller stopped waiting.
                context.check()?;
                Ok(7)
            }
            .boxed_local()
        })
        .unwrap();
    drop(receiver);
    pool.run_until_stalled();
    let context = issued.borrow().clone().unwrap();
    context.check().unwrap();
    release.send(()).unwrap();
    pool.run_until_stalled();
    // The root ended with its operation.
    let ended = context.check().unwrap_err();
    assert_eq!(ended.code, tembra::core::Code::Expired, "{ended}");
    pool.run_until(host.close()).unwrap();
}

#[test]
fn spawned_roots_report_results_and_cancellation() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let issued: Rc<RefCell<Option<Context>>> = Rc::default();
    let cell = issued.clone();
    let receiver = host
        .spawn_root("alice", move |context| {
            *cell.borrow_mut() = Some(context.clone());
            async move {
                context.signal().cancelled().await;
                context.check().map(|()| 0)
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    issued.borrow().clone().unwrap().cancel().unwrap();
    let error = pool.run_until(receiver).unwrap().unwrap_err();
    assert_eq!(error.code, tembra::core::Code::Cancelled, "{error}");
    pool.run_until(host.close()).unwrap();
}
