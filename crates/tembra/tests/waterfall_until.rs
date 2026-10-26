#![cfg(feature = "hooks")]

use futures::{FutureExt, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{
    Error, Hook, Host, Orb, Service,
    core::{ContextKey, Requirement},
};

fn requirement(hook: &str) -> Requirement {
    Requirement {
        orb: "provider".into(),
        api_version: 1,
        methods: Default::default(),
        hooks: [hook.into()].into(),
    }
}

fn install_provider(host: &Host, hook: &Hook<i32, i32>) {
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
}

fn run(pool: &mut LocalPool, host: &Host, input: i32) -> tembra::Result<i32> {
    pool.run_until_stalled();
    let reference = host.get("provider", vec!["main".into()], "alice")?;
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call(&ctx, "run", input).await }.boxed_local()
    }))
}

#[test]
fn transformations_follow_priority_then_registration_order() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let checked = Rc::new(RefCell::new(Vec::new()));
    let captured = checked.clone();
    let hook = Hook::<i32, i32>::waterfall_until("transform", move |value| {
        captured.borrow_mut().push(*value);
        false
    });
    install_provider(&host, &hook);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut orbs = Vec::new();
    for (name, priority, marker) in [("first", 10, 1), ("second", -1, 2), ("third", 10, 3)] {
        let captured = seen.clone();
        orbs.push(
            Orb::new(name, 1)
                .requires(requirement("transform"))
                .contribute("provider", &hook, priority, move |_, value| {
                    captured.borrow_mut().push((marker, value));
                    async move { Ok(value * 10 + marker) }
                }),
        );
    }
    host.install(orbs).unwrap();
    assert_eq!(run(&mut pool, &host, 0).unwrap(), 213);
    assert_eq!(*seen.borrow(), [(2, 0), (1, 2), (3, 21)]);
    assert_eq!(*checked.borrow(), [0, 2, 21, 213]);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().context_stats().0, 0);
    pool.run_until(host.close()).unwrap();
}

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Decision {
    arguments: String,
    block: Option<String>,
}

#[test]
fn first_block_preserves_transformed_arguments_and_skips_later_participants() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<Decision, Decision>::waterfall_until("decide", |value| value.block.is_some());
    let declared = hook.clone();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let transformed = seen.clone();
    let blocked = seen.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared))
                .method("run", |hook, ctx, input: Decision| async move {
                    hook.run(&ctx, input).await
                }),
        ),
        Orb::new("contributors", 1)
            .requires(requirement("decide"))
            .contribute("provider", &hook, -10, move |_, mut value| {
                transformed.borrow_mut().push("transform");
                value.arguments.push_str("-changed");
                async move { Ok(value) }
            })
            .contribute("provider", &hook, 0, move |_, mut value| {
                blocked.borrow_mut().push("block");
                assert_eq!(value.arguments, "original-changed");
                value.block = Some("policy".into());
                async move { Ok(value) }
            })
            .contribute("provider", &hook, 10, |_, _| async {
                panic!("a lower-priority participant must not run after the first block")
            }),
    ])
    .unwrap();
    pool.run_until_stalled();
    let reference = host.get("provider", vec!["main".into()], "alice").unwrap();
    let result: Decision = pool
        .run_until(host.root("alice", move |ctx| {
            async move {
                reference
                    .call(
                        &ctx,
                        "run",
                        Decision {
                            arguments: "original".into(),
                            block: None,
                        },
                    )
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        result,
        Decision {
            arguments: "original-changed".into(),
            block: Some("policy".into()),
        }
    );
    assert_eq!(*seen.borrow(), ["transform", "block"]);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn empty_plans_return_input_and_terminal_input_invokes_no_participants() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let checked = Rc::new(RefCell::new(Vec::new()));
    let captured = checked.clone();
    let hook = Hook::<i32, i32>::waterfall_until("transform", move |value| {
        captured.borrow_mut().push(*value);
        *value < 0
    });
    install_provider(&host, &hook);
    assert_eq!(run(&mut pool, &host, 7).unwrap(), 7);
    assert_eq!(run(&mut pool, &host, -1).unwrap(), -1);
    host.install(vec![
        Orb::new("contributor", 1)
            .requires(requirement("transform"))
            .contribute("provider", &hook, 0, |_, _| async {
                panic!("terminal input must not reach participants")
            }),
    ])
    .unwrap();
    assert_eq!(run(&mut pool, &host, -2).unwrap(), -2);
    assert_eq!(*checked.borrow(), [7, -1, -2]);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn participant_errors_propagate_without_predicate_recheck_or_later_work() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let checked = Rc::new(RefCell::new(Vec::new()));
    let captured = checked.clone();
    let hook = Hook::<i32, i32>::waterfall_until("transform", move |value| {
        captured.borrow_mut().push(*value);
        false
    });
    install_provider(&host, &hook);
    let earlier_effect = Rc::new(Cell::new(0));
    let captured = earlier_effect.clone();
    host.install(vec![
        Orb::new("contributor", 1)
            .requires(requirement("transform"))
            .contribute("provider", &hook, 0, move |_, value| {
                captured.set(captured.get() + 1);
                async move { Ok(value + 1) }
            })
            .contribute("provider", &hook, 1, |_, value| async move {
                assert_eq!(value, 2);
                Err(Error::invalid("participant_failed", "failure evidence"))
            })
            .contribute("provider", &hook, 2, |_, _| async {
                panic!("later participants must not run after an error")
            }),
    ])
    .unwrap();
    let error = run(&mut pool, &host, 1).unwrap_err();
    assert_eq!(error.reason, "participant_failed");
    assert_eq!(error.message, "failure evidence");
    assert_eq!(*checked.borrow(), [1, 2]);
    assert_eq!(earlier_effect.get(), 1);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().context_stats().0, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn ordinary_waterfall_keeps_running_through_all_results() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<i32, i32>::waterfall("transform");
    install_provider(&host, &hook);
    assert_eq!(run(&mut pool, &host, 1).unwrap(), 1);
    host.install(vec![
        Orb::new("contributor", 1)
            .requires(requirement("transform"))
            .contribute(
                "provider",
                &hook,
                0,
                |_, value| async move { Ok(value + 10) },
            )
            .contribute(
                "provider",
                &hook,
                1,
                |_, value| async move { Ok(value + 100) },
            ),
    ])
    .unwrap();
    assert_eq!(run(&mut pool, &host, 1).unwrap(), 111);
    pool.run_until(host.close()).unwrap();
}

#[derive(serde::Deserialize)]
struct NativeValue {
    value: i32,
    #[serde(skip)]
    identity: Option<Rc<()>>,
}

impl serde::Serialize for NativeValue {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        panic!("native hook values must not serialize")
    }
}

#[test]
fn native_values_keep_identity_and_context_across_transformations() {
    const TRACE: ContextKey<String> = ContextKey::new("test/trace", 1);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook =
        Hook::<NativeValue, NativeValue>::waterfall_until("native", |value| value.value >= 42);
    let declared = hook.clone();
    let identity = Rc::new(());
    let initial = identity.clone();
    let first = identity.clone();
    let second = identity.clone();
    let versions = Rc::new(RefCell::new(Vec::new()));
    let first_versions = versions.clone();
    let second_versions = versions.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| self_.hook(&declared)).method(
                "run",
                move |hook, ctx, input: i32| {
                    let initial = initial.clone();
                    async move {
                        let result = hook
                            .run(
                                &ctx.with(&TRACE, "trace".into())?,
                                NativeValue {
                                    value: input,
                                    identity: Some(initial.clone()),
                                },
                            )
                            .await?;
                        assert!(Rc::ptr_eq(result.identity.as_ref().unwrap(), &initial));
                        Ok(result.value)
                    }
                },
            ),
        ),
        Orb::new("contributor", 1)
            .requires(requirement("native"))
            .contribute("provider", &hook, 0, move |ctx, mut value| {
                assert!(Rc::ptr_eq(value.identity.as_ref().unwrap(), &first));
                assert_eq!(ctx.principal().unwrap(), "alice");
                assert_eq!(ctx.value(&TRACE).unwrap(), Some("trace".into()));
                first_versions
                    .borrow_mut()
                    .push(ctx.invocation().unwrap().code_version);
                value.value += 1;
                async move { Ok(value) }
            })
            .contribute("provider", &hook, 1, move |ctx, mut value| {
                assert!(Rc::ptr_eq(value.identity.as_ref().unwrap(), &second));
                assert_eq!(ctx.principal().unwrap(), "alice");
                assert_eq!(ctx.value(&TRACE).unwrap(), Some("trace".into()));
                second_versions
                    .borrow_mut()
                    .push(ctx.invocation().unwrap().code_version);
                value.value += 1;
                async move { Ok(value) }
            })
            .contribute("provider", &hook, 2, |_, _| async {
                panic!("terminal native result must skip later participants")
            }),
    ])
    .unwrap();
    assert_eq!(run(&mut pool, &host, 40).unwrap(), 42);
    assert_eq!(*versions.borrow(), [host.kernel().version(); 2]);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().context_stats().0, 0);
    pool.run_until(host.close()).unwrap();
}
