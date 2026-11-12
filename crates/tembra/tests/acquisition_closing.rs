//! Waiting acquisition must never overlap the previous instance's physical resource close.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{AcquisitionOptions, Context, Host, Orb, Reference, Service};

struct Closing {
    host: Host,
    created: Rc<Cell<usize>>,
    release: oneshot::Sender<()>,
    previous: u64,
}

fn closing(pool: &mut LocalPool) -> Closing {
    closing_with_error(pool, false)
}

fn closing_with_error(pool: &mut LocalPool, fail_close: bool) -> Closing {
    let host = Host::new(pool.spawner());
    let created = Rc::new(Cell::new(0));
    let (send, receive) = oneshot::channel::<()>();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.install(vec![Orb::new("store", 1).service(Service::new(
        "connection",
        {
            let created = created.clone();
            move |declaration| {
                created.set(created.get() + 1);
                let receive = Rc::new(RefCell::new(receive.borrow_mut().take()));
                declaration.resource(
                    "link",
                    1,
                    || Ok(()),
                    move |_, _| {
                        let receive = receive.borrow_mut().take();
                        async move {
                            if let Some(receive) = receive {
                                let _ = receive.await;
                                if fail_close {
                                    return Err(tembra::Error::invalid(
                                        "close_failure",
                                        "old close failed",
                                    ));
                                }
                            }
                            Ok(())
                        }
                        .boxed_local()
                    },
                )
            }
        },
    ))])
    .unwrap();
    let previous = host
        .get("connection", vec!["a".into()], "personal")
        .unwrap();
    pool.run_until(previous.ready()).unwrap();
    let object = previous.object();
    previous.release().unwrap();
    pool.run_until_stalled();
    assert_eq!(host.kernel().diagnostics().objects, 1);
    Closing {
        host,
        created,
        release: send,
        previous: object,
    }
}

async fn acquire(context: &Context) -> tembra::Result<Rc<Reference>> {
    context
        .get_wait(
            "connection",
            vec!["a".into()],
            AcquisitionOptions::default(),
        )
        .await
}

#[test]
fn acquisition_waits_for_physical_close_without_overlapping_instances() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let mut next = Box::pin(closing.host.root("personal", |context| {
        async move { acquire(&context).await }.boxed_local()
    }));
    pool.run_until(futures::future::poll_fn(|cx| {
        assert!(
            next.as_mut().poll(cx).is_pending(),
            "a closing address must remain pending until physical disposal"
        );
        std::task::Poll::Ready(())
    }));
    assert_eq!(closing.created.get(), 1);
    closing.release.send(()).unwrap();
    let next = pool.run_until(next).unwrap();
    assert_ne!(next.object(), closing.previous);
    assert_eq!(closing.created.get(), 2);
    pool.run_until(closing.host.close()).unwrap();
}

fn assert_pending<T>(
    pool: &mut LocalPool,
    future: std::pin::Pin<&mut impl std::future::Future<Output = T>>,
) {
    let mut future = future;
    pool.run_until(futures::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    }));
}

#[test]
fn concurrent_waiters_share_one_replacement_and_keep_independent_claims() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let mut first = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions::default(),
    ));
    let mut second = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions::default(),
    ));
    assert_pending(&mut pool, first.as_mut());
    assert_pending(&mut pool, second.as_mut());
    assert_eq!(closing.created.get(), 1);
    assert_eq!(closing.host.kernel().diagnostics().references, 0);
    closing.release.send(()).unwrap();
    let (first, second) = pool.run_until(futures::future::join(first, second));
    let first = first.unwrap();
    let second = second.unwrap();
    pool.run_until(first.ready()).unwrap();
    pool.run_until(second.ready()).unwrap();
    assert_eq!(closing.created.get(), 2);
    assert_eq!(first.object(), second.object());
    assert_ne!(first.id(), second.id());
    assert_ne!(first.object(), closing.previous);
    first.release().unwrap();
    second.release().unwrap();
    pool.run_until(closing.host.close()).unwrap();
    assert_eq!(closing.host.kernel().diagnostics().objects, 0);
}

#[test]
fn other_addresses_and_impossible_requirements_do_not_wait() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let key = vec!["a".into()];
    let error = closing
        .host
        .get("connection", key.clone(), "personal")
        .err()
        .unwrap();
    assert_eq!(error.reason, "disposing");
    let different_key = pool
        .run_until(closing.host.get_wait(
            "connection",
            vec!["b".into()],
            "personal",
            AcquisitionOptions::default(),
        ))
        .unwrap();
    let different_principal = pool
        .run_until(closing.host.get_wait(
            "connection",
            key.clone(),
            "work",
            AcquisitionOptions::default(),
        ))
        .unwrap();
    assert_eq!(closing.created.get(), 3);
    let error = pool
        .run_until(closing.host.get_wait(
            "connection",
            key.clone(),
            "personal",
            AcquisitionOptions {
                api_version: 9,
                ..Default::default()
            },
        ))
        .err()
        .unwrap();
    assert_eq!(error.reason, "api_version");
    let error = pool
        .run_until(closing.host.get_wait(
            "connection",
            key,
            "personal",
            AcquisitionOptions {
                methods: ["missing".into()].into(),
                ..Default::default()
            },
        ))
        .err()
        .unwrap();
    assert_eq!(error.reason, "missing_method");
    assert_eq!(closing.created.get(), 3);
    different_key.release().unwrap();
    different_principal.release().unwrap();
    closing.release.send(()).unwrap();
    pool.run_until(closing.host.close()).unwrap();
}

#[test]
fn root_cancellation_stops_waiting_without_claiming_or_cancelling_cleanup() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    pool.run_until(closing.host.root("personal", |context| {
        async move {
            let mut waiting = Box::pin(acquire(&context));
            futures::future::poll_fn(|cx| {
                assert!(waiting.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            context.cancel()?;
            let error = waiting.await.err().unwrap();
            assert_eq!(error.code, tembra::core::Code::Cancelled);
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(closing.created.get(), 1);
    assert_eq!(closing.host.kernel().diagnostics().references, 0);
    assert_eq!(closing.host.kernel().diagnostics().objects, 1);
    closing.release.send(()).unwrap();
    pool.run_until(closing.host.settled());
    assert_eq!(closing.created.get(), 1);
    pool.run_until(closing.host.close()).unwrap();
}

#[test]
fn ended_root_wakes_a_retained_acquisition_without_creating_a_replacement() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let (issued, context) = oneshot::channel();
    let (leave, leaving) = oneshot::channel::<()>();
    let mut root = Box::pin(closing.host.root("personal", |context| {
        async move {
            issued.send(context).ok();
            let _ = leaving.await;
            Ok(())
        }
        .boxed_local()
    }));
    assert_pending(&mut pool, root.as_mut());
    let context = pool.run_until(context).unwrap();
    let mut waiting = Box::pin(acquire(&context));
    assert_pending(&mut pool, waiting.as_mut());
    leave.send(()).unwrap();
    pool.run_until(root).unwrap();
    let error = pool.run_until(waiting).err().unwrap();
    assert_eq!(error.code, tembra::core::Code::Expired);
    assert_eq!(closing.created.get(), 1);
    assert_eq!(closing.host.kernel().diagnostics().references, 0);
    closing.release.send(()).unwrap();
    pool.run_until(closing.host.close()).unwrap();
}

#[test]
fn dropped_acquisition_and_host_shutdown_leave_no_replacement() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let mut abandoned = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions::default(),
    ));
    assert_pending(&mut pool, abandoned.as_mut());
    drop(abandoned);
    let mut waiting = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions::default(),
    ));
    assert_pending(&mut pool, waiting.as_mut());
    let mut shutdown = Box::pin(closing.host.close());
    assert_pending(&mut pool, shutdown.as_mut());
    let error = pool.run_until(waiting).err().unwrap();
    assert_eq!(error.code, tembra::core::Code::Closed);
    assert_eq!(closing.created.get(), 1);
    closing.release.send(()).unwrap();
    pool.run_until(shutdown).unwrap();
    assert_eq!(closing.host.kernel().diagnostics().objects, 0);
}

#[test]
fn provider_requirements_are_revalidated_after_disposal() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let mut waiting = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions {
            api_version: 1,
            ..Default::default()
        },
    ));
    assert_pending(&mut pool, waiting.as_mut());
    let replacements = Rc::new(Cell::new(0));
    let replacement = Orb::new("store", 2).service(Service::new("connection", {
        let replacements = replacements.clone();
        move |_| {
            replacements.set(replacements.get() + 1);
            Ok(())
        }
    }));
    closing.release.send(()).unwrap();
    pool.run_until(closing.host.reload(vec![replacement], 10_000))
        .unwrap();
    let error = pool.run_until(waiting).err().unwrap();
    assert_eq!(error.reason, "api_version");
    assert_eq!(closing.created.get(), 1);
    assert_eq!(replacements.get(), 0);
    let reference = pool
        .run_until(closing.host.get_wait(
            "connection",
            vec!["a".into()],
            "personal",
            AcquisitionOptions {
                api_version: 2,
                ..Default::default()
            },
        ))
        .unwrap();
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(replacements.get(), 1);
    reference.release().unwrap();
    pool.run_until(closing.host.close()).unwrap();
}

#[test]
fn old_cleanup_failure_is_reported_once_and_does_not_revive_the_old_instance() {
    let mut pool = LocalPool::new();
    let closing = closing_with_error(&mut pool, true);
    let mut waiting = Box::pin(closing.host.get_wait(
        "connection",
        vec!["a".into()],
        "personal",
        AcquisitionOptions::default(),
    ));
    assert_pending(&mut pool, waiting.as_mut());
    closing.release.send(()).unwrap();
    let replacement = pool.run_until(waiting).unwrap();
    pool.run_until(replacement.ready()).unwrap();
    assert_ne!(replacement.object(), closing.previous);
    assert_eq!(closing.created.get(), 2);
    let errors = closing.host.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].reason, "close_failure");
    replacement.release().unwrap();
    pool.run_until(closing.host.close()).unwrap();
    assert!(closing.host.errors().is_empty());
}

#[test]
fn normal_root_end_wakes_an_entered_acquisition_without_cancelling_admitted_work() {
    let mut pool = LocalPool::new();
    let closing = closing(&mut pool);
    let (entered, enter) = oneshot::channel();
    let (physical, physically_done) = oneshot::channel::<()>();
    let entered = Rc::new(RefCell::new(Some(entered)));
    let physically_done = Rc::new(RefCell::new(Some(physically_done)));
    closing
        .host
        .install(vec![Orb::new("worker", 1).service(
            Service::new("physical", |_| Ok(())).method("hold", move |_, context, (): ()| {
                let entered = entered.borrow_mut().take().unwrap();
                let physically_done = physically_done.borrow_mut().take().unwrap();
                async move {
                    let signal = context.signal();
                    let _ = entered.send((context, signal.clone()));
                    physically_done.await.unwrap();
                    signal.check()?;
                    Ok(())
                }
            }),
        )])
        .unwrap();
    let reference = closing
        .host
        .get("physical", vec!["main".into()], "personal")
        .unwrap();
    pool.run_until(reference.ready()).unwrap();
    let (leave, leaving) = oneshot::channel::<()>();
    let mut root = Box::pin(closing.host.root("personal", |context| {
        async move {
            let issued = reference
                .issue_portable::<_, ()>(&context, "hold", ())
                .await?;
            leaving.await.unwrap();
            Ok(issued)
        }
        .boxed_local()
    }));
    assert_pending(&mut pool, root.as_mut());
    let (context, admitted_signal) = pool.run_until(enter).unwrap();
    let mut waiting = Box::pin(acquire(&context));
    assert_pending(&mut pool, waiting.as_mut());
    leave.send(()).unwrap();
    let (call, result) = pool.run_until(root).unwrap();
    let error = pool.run_until(waiting).err().unwrap();
    assert_eq!(error.code, tembra::core::Code::Expired);
    assert!(!admitted_signal.is_cancelled());
    assert_eq!(closing.host.kernel().diagnostics().calls, 1);
    assert_eq!(closing.created.get(), 1);
    physical.send(()).unwrap();
    pool.run_until(result).unwrap();
    pool.run_until(call.settled());
    assert_eq!(closing.host.kernel().diagnostics().calls, 0);
    closing.release.send(()).unwrap();
    pool.run_until(closing.host.close()).unwrap();
    assert!(closing.host.errors().is_empty());
}

#[test]
fn normal_root_end_preserves_the_retained_root_cancellation_signal() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (context, signal) = pool
        .run_until(host.root("personal", |context| {
            async move {
                let signal = context.signal();
                Ok((context, signal))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        context.check().unwrap_err().code,
        tembra::core::Code::Expired
    );
    assert!(
        !signal.is_cancelled(),
        "logical root completion must not cancel retained physical work"
    );
    pool.run_until(host.close()).unwrap();
}
