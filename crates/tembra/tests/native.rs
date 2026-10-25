use futures::{FutureExt, executor::LocalPool};
use std::{cell::Cell, rc::Rc};
use tembra::{Error, Facet, Host, Service};

#[test]
fn native_typed_values_validation_and_cleanup() {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let closed = Rc::new(Cell::new(0));
    let retained = closed.clone();
    let service = Service::new("counter", move |self_| {
        let closed = retained.clone();
        let connection = self_.resource(
            "connection",
            1,
            || Ok(()),
            move |_, _| {
                async move {
                    closed.set(closed.get() + 1);
                    Ok(())
                }
                .boxed_local()
            },
        )?;
        Ok((
            self_.state(0_i32, 1, |value| {
                if (0..=100).contains(value) {
                    Ok(())
                } else {
                    Err(Error::invalid("range", "outside allowed range"))
                }
            })?,
            connection,
        ))
    })
    .method("set", |environment, context, (value,): (i32,)| async move {
        Ok(*environment.0.update(&context, |_| Ok(value))?)
    });
    host.install(vec![Facet::new("counter", 1).service(service)])
        .unwrap();
    let reference = Rc::new(host.get("counter", vec!["main".into()], "host").unwrap());
    executor
        .run_until(host.root("host", {
            let reference = reference.clone();
            move |context| {
                async move {
                    assert_eq!(
                        reference.call::<_, i32>(&context, "set", (42_i32,)).await?,
                        42
                    );
                    assert_eq!(
                        reference
                            .call::<_, i32>(&context, "set", (101_i32,))
                            .await
                            .unwrap_err()
                            .reason,
                        "range"
                    );
                    Ok(())
                }
                .boxed_local()
            }
        }))
        .unwrap();
    reference.release().unwrap();
    executor.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
    assert_eq!(host.kernel().diagnostics().objects, 0);
}

#[test]
fn native_resources_open_through_the_engine_and_failed_activation_can_retry() {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let opens = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let attempts = Rc::new(Cell::new(0));
    let service = Service::new("native.resources", {
        let (opens, closes, attempts) = (opens.clone(), closes.clone(), attempts.clone());
        move |declaration| {
            let (opened, closed) = (opens.clone(), closes.clone());
            let base = declaration.resource(
                "base",
                1,
                move || {
                    opened.set(opened.get() + 1);
                    Ok(42_u32)
                },
                move |_, _| {
                    async move {
                        closed.set(closed.get() + 1);
                        Ok(())
                    }
                    .boxed_local()
                },
            )?;
            assert!(base.value().is_err(), "factories only declare resources");
            let attempt = attempts.clone();
            declaration.resource_async(
                "dependent",
                1,
                &[base.key()],
                move |_| {
                    async move {
                        let value = *base.value()?;
                        attempt.set(attempt.get() + 1);
                        if attempt.get() == 1 {
                            return Err(Error::invalid("open_once", "retryable opener failure"));
                        }
                        Ok(value)
                    }
                    .boxed_local()
                },
                |_, _| async { Ok(()) }.boxed_local(),
            )
        }
    })
    .method("read", |environment, _, (): ()| async move {
        Ok(*environment.value()?)
    });
    host.install(vec![Facet::new("native.resources", 1).service(service)])
        .unwrap();
    let first = host
        .get("native.resources", vec!["main".into()], "host")
        .unwrap();
    assert_eq!(
        executor.run_until(first.ready()).unwrap_err().reason,
        "open_once"
    );
    executor.run_until_stalled();
    assert_eq!(closes.get(), 1);
    assert_eq!(host.kernel().diagnostics().objects, 0);
    let next = Rc::new(
        host.get("native.resources", vec!["main".into()], "host")
            .unwrap(),
    );
    executor.run_until(next.ready()).unwrap();
    let value = executor
        .run_until(host.root("host", {
            let next = next.clone();
            move |context| {
                async move { next.call::<_, u32>(&context, "read", ()).await }.boxed_local()
            }
        }))
        .unwrap();
    assert_eq!(value, 42);
    first.release().unwrap();
    next.release().unwrap();
    executor.run_until(host.close()).unwrap();
    assert_eq!(opens.get(), 2);
    assert_eq!(closes.get(), 2);
    assert!(host.errors().is_empty());
}

#[test]
fn native_shutdown_signals_openers_but_waits_for_physical_settlement() {
    use futures::channel::oneshot;
    use std::{cell::RefCell, future::Future, task::Poll};
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let cancelled = Rc::new(Cell::new(false));
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.install(vec![Facet::new("pending", 1).service(Service::new(
        "pending",
        {
            let cancelled = cancelled.clone();
            move |declaration| {
                let cancelled = cancelled.clone();
                let receive = receive.borrow_mut().take().unwrap();
                declaration.resource_async(
                    "pending",
                    1,
                    &[],
                    move |signal| {
                        async move {
                            signal.cancelled().await;
                            cancelled.set(true);
                            receive.await.unwrap();
                            signal.check()?;
                            Ok(())
                        }
                        .boxed_local()
                    },
                    |_, _| async { Ok(()) }.boxed_local(),
                )
            }
        },
    ))])
    .unwrap();
    let reference = host.get("pending", vec!["main".into()], "host").unwrap();
    executor.run_until_stalled();
    let mut closing = Box::pin(host.close());
    executor.run_until(futures::future::poll_fn(|context| {
        assert!(closing.as_mut().poll(context).is_pending());
        Poll::Ready(())
    }));
    executor.run_until_stalled();
    assert!(cancelled.get());
    assert_eq!(host.kernel().diagnostics().resources, 1);
    send.send(()).unwrap();
    executor.run_until(closing).unwrap();
    assert_eq!(host.kernel().diagnostics().resources, 0);
    assert_eq!(host.kernel().diagnostics().objects, 0);
    drop(reference);
}
