#![cfg(feature = "hooks")]
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{
    Context, Error, Hook, Host, Orb, Service,
    core::{ContextKey, Requirement},
};

fn requirement(hooks: &[&str]) -> Requirement {
    Requirement {
        orb: "provider".into(),
        api_version: 1,
        methods: Default::default(),
        hooks: hooks.iter().map(|hook| (*hook).into()).collect(),
    }
}

fn run<A: serde::Serialize + 'static, R: serde::de::DeserializeOwned + 'static>(
    pool: &mut LocalPool,
    host: &Host,
    method: &'static str,
    input: A,
) -> tembra::Result<R> {
    pool.run_until_stalled();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice")?);
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call(&ctx, method, input).await }.boxed_local()
    }))
}

#[test]
fn compositions_order_transformation_and_first_decision() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let series = Hook::<i32, ()>::series("observe");
    let waterfall = Hook::<i32, i32>::waterfall("transform");
    let bail = Hook::<i32, Option<i32>>::bail("decide");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let provider = Orb::new("provider", 1)
        .hook(series.clone())
        .hook(waterfall.clone())
        .hook(bail.clone())
        .service(
            Service::new("provider", move |self_| {
                Ok((
                    self_.hook(&series)?,
                    self_.hook(&waterfall)?,
                    self_.hook(&bail)?,
                ))
            })
            .method("observe", |env, ctx, input: i32| async move {
                env.0.run(&ctx, input).await
            })
            .method("transform", |env, ctx, input: i32| async move {
                env.1.run(&ctx, input).await
            })
            .method("decide", |env, ctx, input: i32| async move {
                env.2.run(&ctx, input).await
            }),
        );
    host.install(vec![provider]).unwrap();
    assert_eq!(run::<_, i32>(&mut pool, &host, "transform", 3).unwrap(), 3);
    assert_eq!(
        run::<_, Option<i32>>(&mut pool, &host, "decide", 3).unwrap(),
        None
    );
    let mut orbs = Vec::new();
    for (name, priority, marker) in [("first", 10, 1), ("second", -1, 2), ("third", 10, 3)] {
        let captured = seen.clone();
        orbs.push(
            Orb::new(name, 1)
                .requires(requirement(&["observe", "transform", "decide"]))
                .contribute(
                    "provider",
                    &Hook::<i32, ()>::series("observe"),
                    priority,
                    move |_, input| {
                        captured.borrow_mut().push((marker, input));
                        async { Ok(()) }
                    },
                )
                .contribute(
                    "provider",
                    &Hook::<i32, i32>::waterfall("transform"),
                    priority,
                    move |_, input| async move { Ok(input * 10 + marker) },
                )
                .contribute(
                    "provider",
                    &Hook::<i32, Option<i32>>::bail("decide"),
                    priority,
                    move |_, _| async move { Ok((marker != 2).then_some(marker)) },
                ),
        );
    }
    host.install(orbs).unwrap();
    run::<_, ()>(&mut pool, &host, "observe", 7).unwrap();
    assert_eq!(*seen.borrow(), [(2, 7), (1, 7), (3, 7)]);
    assert_eq!(
        run::<_, i32>(&mut pool, &host, "transform", 0).unwrap(),
        213
    );
    assert_eq!(
        run::<_, Option<i32>>(&mut pool, &host, "decide", 0).unwrap(),
        Some(1)
    );
    pool.run_until(host.close()).unwrap();
}

#[test]
fn errors_stop_the_plan_and_invalid_dependencies_do_not_install() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<i32, i32>::waterfall("transform");
    let declared = hook.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared))
                .method("run", |hook, ctx, input: i32| async move {
                    hook.run(&ctx, input).await
                }),
        ),
    ])
    .unwrap();
    assert_eq!(
        host.install(vec![Orb::new("bad", 1).contribute(
            "provider",
            &hook,
            0,
            |_, value| async move { Ok(value) }
        )])
        .unwrap_err()
        .reason,
        "hook_dependency"
    );
    assert_eq!(host.kernel().installed_orbs().len(), 1);
    let wrong = Hook::<String, String>::waterfall("transform");
    assert_eq!(
        host.install(vec![
            Orb::new("bad", 1)
                .requires(requirement(&["transform"]))
                .contribute("provider", &wrong, 0, |_, value| async move { Ok(value) })
        ])
        .unwrap_err()
        .reason,
        "rust_type"
    );
    assert!(
        host.install(vec![Orb::new("bad", 1).requires(requirement(&["missing"]))])
            .is_err()
    );
    let called = Rc::new(Cell::new(false));
    let captured = called.clone();
    host.install(vec![
        Orb::new("errors", 1)
            .requires(requirement(&["transform"]))
            .contribute("provider", &hook, 0, |_, _| async {
                Err(Error::invalid("test_failure", "failed"))
            })
            .contribute("provider", &hook, 1, move |_, value| {
                captured.set(true);
                async move { Ok(value) }
            }),
    ])
    .unwrap();
    assert_eq!(
        run::<_, i32>(&mut pool, &host, "run", 1)
            .unwrap_err()
            .reason,
        "test_failure"
    );
    assert!(!called.get());
    assert_eq!(host.kernel().diagnostics().calls, 0);
    pool.run_until(host.close()).unwrap();
}

#[derive(serde::Deserialize)]
struct LocalOnly(i32);
impl serde::Serialize for LocalOnly {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        panic!("local hooks must not serialize")
    }
}

#[test]
fn local_hooks_never_serialize_and_keep_context_version_and_principal() {
    const TRACE: ContextKey<String> = ContextKey::new("test/trace", 1);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<LocalOnly, LocalOnly>::waterfall("local");
    let declared = hook.clone();
    let version = Rc::new(Cell::new(0));
    let captured = version.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared)).method(
                "run",
                |hook, ctx, value: i32| async move {
                    Ok(hook
                        .run(&ctx.with(&TRACE, "trace".into())?, LocalOnly(value))
                        .await?
                        .0)
                },
            ),
        ),
        Orb::new("contributor", 1)
            .requires(requirement(&["local"]))
            .contribute("provider", &hook, 0, move |ctx, value| {
                captured.set(ctx.invocation().unwrap().code_version);
                assert_eq!(ctx.principal().unwrap(), "alice");
                assert_eq!(ctx.value(&TRACE).unwrap(), Some("trace".into()));
                async move { Ok(LocalOnly(value.0 + 1)) }
            }),
    ])
    .unwrap();
    assert_eq!(run::<_, i32>(&mut pool, &host, "run", 41).unwrap(), 42);
    assert_eq!(version.get(), host.kernel().version());
    assert_eq!(host.kernel().context_stats().0, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn dropped_wait_and_shutdown_retain_active_contribution_and_frames() {
    use std::{future::Future, task::Poll};
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<i32, i32>::waterfall("pending");
    let declared = hook.clone();
    let (send, receive) = oneshot::channel();
    let receive = RefCell::new(Some(receive));
    let entered = Rc::new(RefCell::new(None::<Context>));
    let captured = entered.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared))
                .method("run", |hook, ctx, value: i32| async move {
                    hook.run(&ctx, value).await
                }),
        ),
        Orb::new("pending", 1)
            .requires(requirement(&["pending"]))
            .contribute("provider", &hook, 0, move |ctx, value| {
                *captured.borrow_mut() = Some(ctx);
                let receive = receive.borrow_mut().take().unwrap();
                async move {
                    receive.await.unwrap();
                    Ok(value)
                }
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let mut waiting = Box::pin(host.root("alice", {
        let reference = reference.clone();
        move |ctx| async move { reference.call::<_, i32>(&ctx, "run", 1).await }.boxed_local()
    }));
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(waiting.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    pool.run_until_stalled();
    drop(waiting);
    reference.release().unwrap();
    assert_eq!(host.kernel().diagnostics().calls, 3);
    let mut closing = Box::pin(host.close());
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(closing.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    assert_eq!(host.kernel().diagnostics().objects, 1);
    let context = entered.borrow().clone().unwrap();
    assert_eq!(context.check().unwrap_err().code, tembra::Code::Expired);
    assert!(
        host.kernel()
            .invocation(context.invocation().unwrap().id)
            .is_ok()
    );
    send.send(()).unwrap();
    pool.run_until(closing).unwrap();
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().diagnostics().objects, 0);
}

#[test]
fn contributions_wait_for_their_orb_resources_before_entering() {
    use std::{future::Future, task::Poll};
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<i32, i32>::waterfall("resource");
    let declared = hook.clone();
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared))
                .method("run", |hook, ctx, input: i32| async move {
                    hook.run(&ctx, input).await
                }),
        ),
        Orb::new("contributor", 1)
            .requires(requirement(&["resource"]))
            .create(move |self_| {
                let receive = receive.borrow_mut().take().unwrap();
                self_.resource_async(
                    "value",
                    1,
                    &[],
                    move |_| {
                        async move {
                            receive
                                .await
                                .map_err(|_| Error::invalid("source", "source closed"))
                        }
                        .boxed_local()
                    },
                    |_, _| async { Ok(()) }.boxed_local(),
                )
            })
            .contribute("provider", &hook, 0, |ctx, input| async move {
                Ok(input + *ctx.orb::<tembra::Resource<i32>>()?.value()?)
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let mut waiting = Box::pin(host.root("alice", move |ctx| {
        async move { reference.call::<_, i32>(&ctx, "run", 1_i32).await }.boxed_local()
    }));
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(waiting.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    pool.run_until_stalled();
    send.send(41).unwrap();
    assert_eq!(pool.run_until(waiting).unwrap(), 42);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn roots_keep_their_plan_version_when_new_contributors_are_installed() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<i32, i32>::waterfall("versioned");
    let declared = hook.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared))
                .method("run", |hook, ctx, input: i32| async move {
                    hook.run(&ctx, input).await
                }),
        ),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let retained = host.clone();
    let entered = reference.clone();
    pool.run_until(host.root("alice", move |ctx| {
        async move {
            retained.install(vec![
                Orb::new("later", 1)
                    .requires(requirement(&["versioned"]))
                    .contribute(
                        "provider",
                        &hook,
                        0,
                        |_, input| async move { Ok(input + 1) },
                    ),
            ])?;
            assert_eq!(entered.call::<_, i32>(&ctx, "run", 41_i32).await?, 41);
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(
        pool.run_until(host.root("alice", {
            let reference = reference.clone();
            move |ctx| {
                async move { reference.call::<_, i32>(&ctx, "run", 41_i32).await }.boxed_local()
            }
        }))
        .unwrap(),
        42
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
