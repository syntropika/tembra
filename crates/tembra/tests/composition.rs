use futures::{FutureExt, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{AcquisitionOptions, Host, Orb, Service, core::Requirement};

#[test]
fn managed_dependencies_wait_share_orb_data_and_release_after_the_consumer() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let closed = Rc::new(RefCell::new(Vec::new()));
    let observed = closed.clone();
    host.declare(move |self_| {
        self_.resource(
            "host",
            1,
            || Ok(42),
            move |_, _| {
                async move {
                    observed.borrow_mut().push("host");
                    Ok(())
                }
                .boxed_local()
            },
        )
    })
    .unwrap();
    let observed = closed.clone();
    let provider = Orb::new("provider", 1)
        .create(|self_| self_.host_resource::<i32>("host"))
        .service(
            Service::new("provider", move |self_| {
                let data = self_.orb::<tembra::Resource<i32>>()?;
                let observed = observed.clone();
                self_.resource(
                    "provider",
                    1,
                    || Ok(()),
                    move |_, _| {
                        async move {
                            observed.borrow_mut().push("provider");
                            Ok(())
                        }
                        .boxed_local()
                    },
                )?;
                Ok(data)
            })
            .method("read", |data, _, (): ()| async move { Ok(*data.value()?) }),
        );
    let observed = closed.clone();
    let client = Orb::new("client", 1)
        .requires(Requirement {
            orb: "provider".into(),
            api_version: 1,
            methods: [("provider".into(), ["read".into()].into())].into(),
            hooks: Default::default(),
        })
        .service(
            Service::new("client", move |self_| {
                let provider = self_.get("provider", vec!["main".into()])?;
                let observed = observed.clone();
                let connection = provider.clone();
                self_.resource(
                    "client",
                    1,
                    || Ok(()),
                    move |_, ctx| {
                        async move {
                            assert_eq!(connection.call::<_, i32>(&ctx, "read", ()).await?, 42);
                            observed.borrow_mut().push("client");
                            Ok(())
                        }
                        .boxed_local()
                    },
                )?;
                Ok(provider)
            })
            .method("read", |provider, ctx, (): ()| async move {
                provider.call::<_, i32>(&ctx, "read", ()).await
            }),
        );
    host.install(vec![client, provider]).unwrap();
    let client = Rc::new(host.get("client", vec!["main".into()], "alice").unwrap());
    assert_eq!(
        pool.run_until(host.root("alice", {
            let client = client.clone();
            move |ctx| async move { client.call::<_, i32>(&ctx, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        42
    );
    client.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(*closed.borrow(), ["client", "provider", "host"]);
}

#[test]
fn root_owned_claims_revoke_handles_and_task_cancellation_waits_for_physical_completion() {
    use futures::channel::oneshot;
    use std::{future::Future, task::Poll};
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let cancelled = Rc::new(Cell::new(false));
    let captured = cancelled.clone();
    let (send, receive) = oneshot::channel();
    let receive = RefCell::new(Some(receive));
    let (started, starting) = oneshot::channel();
    let started = RefCell::new(Some(started));
    host.install(vec![
        Orb::new("task", 1).service(
            Service::new("task", move |self_| {
                let receive = receive.borrow_mut().take().unwrap();
                let captured = captured.clone();
                let started = started.borrow_mut().take().unwrap();
                self_.task(move |task| {
                    async move {
                        task.root(|ctx| {
                            async move {
                                assert_eq!(ctx.principal()?, "alice");
                                Ok(())
                            }
                            .boxed_local()
                        })
                        .await?;
                        started.send(()).unwrap();
                        task.signal().cancelled().await;
                        captured.set(true);
                        receive.await.unwrap();
                        Ok(())
                    }
                    .boxed_local()
                })?;
                Ok(())
            })
            .method("wait", |_, ctx, (): ()| async move {
                ctx.signal().cancelled().await;
                Ok(())
            }),
        ),
    ])
    .unwrap();
    let leaked = pool
        .run_until(host.root("alice", move |ctx| {
            async move {
                let reference =
                    ctx.get("task", vec!["main".into()], AcquisitionOptions::default())?;
                reference.ready().await?;
                starting.await.unwrap();
                Ok(reference)
            }
            .boxed_local()
        }))
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(leaked.status().unwrap(), tembra::core::Status::Released);
    let mut closing = Box::pin(host.close());
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(closing.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    pool.run_until_stalled();
    assert!(cancelled.get());
    assert_eq!(host.kernel().diagnostics().tasks, 1);
    send.send(()).unwrap();
    pool.run_until(closing).unwrap();
    assert_eq!(host.kernel().diagnostics().tasks, 0);
}

#[test]
fn dependency_cycles_are_rejected_before_recursive_activation() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let orb = Orb::new("cycle", 1)
        .service(Service::new("a", |self_| {
            self_.get("b", vec!["main".into()])
        }))
        .service(Service::new("b", |self_| {
            self_.get("a", vec!["main".into()])
        }));
    host.install(vec![orb]).unwrap();
    assert_eq!(
        host.get("a", vec!["main".into()], "alice")
            .err()
            .unwrap()
            .reason,
        "dependency_cycle"
    );
    pool.run_until(host.close()).unwrap();
}

#[cfg(feature = "views")]
#[test]
fn required_versions_methods_and_principal_views_restrict_native_calls() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("restricted", 1).service(
            Service::new("restricted", |_| Ok(()))
                .method("read", |_, _, (): ()| async { Ok(1_i32) })
                .method("write", |_, _, (): ()| async { Ok(2_i32) })
                .access(|principal| {
                    if principal == "admin" {
                        ["read".into(), "write".into()].into()
                    } else {
                        ["read".into()].into()
                    }
                }),
        ),
    ])
    .unwrap();
    assert_eq!(
        host.get_with_options(
            "restricted",
            vec!["main".into()],
            "reader",
            AcquisitionOptions {
                api_version: 2,
                methods: Default::default()
            }
        )
        .err()
        .unwrap()
        .reason,
        "api_version"
    );
    let reference = Rc::new(
        host.get_with_options(
            "restricted",
            vec!["main".into()],
            "reader",
            AcquisitionOptions {
                api_version: 1,
                methods: ["read".into(), "write".into()].into(),
            },
        )
        .unwrap(),
    );
    pool.run_until(host.root("reader", {
        let reference = reference.clone();
        move |ctx| {
            async move {
                assert_eq!(reference.call::<_, i32>(&ctx, "read", ()).await?, 1);
                assert_eq!(
                    reference
                        .call::<_, i32>(&ctx, "write", ())
                        .await
                        .unwrap_err()
                        .code,
                    tembra::Code::Forbidden
                );
                Ok(())
            }
            .boxed_local()
        }
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
