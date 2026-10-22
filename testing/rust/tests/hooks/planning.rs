mod support;
use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use support::*;
use tembra_core::{contracts::Contract, *};
use tembra_hooks::*;

#[test]
fn one_immutable_registry_plan_excludes_contributions_published_mid_run() {
    let h = Harness::new();
    let d = definition(
        "snapshot",
        Strategy::Series,
        integer(),
        HookType::absent(),
        None,
    );
    let (release, gate) = oneshot::channel::<()>();
    let gate = Rc::new(RefCell::new(Some(gate)));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let selected = seen.clone();
    h.add(
        &d,
        "first",
        0,
        method(move |_: HookArgs<i32, ()>, _| {
            selected.borrow_mut().push("first");
            let gate = gate.borrow_mut().take();
            async move {
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                Ok(())
            }
            .boxed_local()
        }),
    );
    h.ready();
    let output = Rc::new(RefCell::new(None));
    let sink = output.clone();
    let (m, old, host) = (h.manager.clone(), d.clone(), h.host.clone());
    h.driver.spawn(
        async move {
            *sink.borrow_mut() = Some(
                host.root(move |ctx| m.run_dynamic_series(&old, 1, &ctx).boxed_local())
                    .await,
            );
        }
        .boxed_local(),
    );
    h.driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &["first"]);
    let selected = seen.clone();
    h.add(
        &d,
        "later",
        1,
        method(move |_: HookArgs<i32, ()>, _| {
            selected.borrow_mut().push("later");
            async { Ok(()) }.boxed_local()
        }),
    );
    h.ready();
    release.send(()).unwrap();
    h.driver.run_until_stalled();
    output.borrow_mut().take().unwrap().unwrap();
    assert_eq!(&*seen.borrow(), &["first"]);
    let m = h.manager.clone();
    h.run(move |ctx| m.run_dynamic_series(&d, 1, &ctx)).unwrap();
    assert_eq!(&*seen.borrow(), &["first", "first", "later"]);
    h.close();
}

#[test]
fn native_middleware_keeps_actual_rc_identity_without_clone_bounds_on_payload() {
    struct Marker;
    let movable = HookType::<Marker>::native("owned-marker@1", |_| Ok(())).unwrap();
    let nonclone = definition(
        "nonclone",
        Strategy::Middleware,
        movable.clone(),
        movable.clone(),
        Some(TerminalProfile {
            input: movable.profile().clone(),
            result: movable.profile().clone(),
        }),
    );
    drop(nonclone.clone()); // RustHookDefinition does not require Marker: Clone.
    let h = Harness::new();
    let ty = HookType::<Rc<Marker>>::native("marker@1", |_| Ok(())).unwrap();
    let d = definition(
        "native",
        Strategy::Middleware,
        ty.clone(),
        ty.clone(),
        Some(TerminalProfile {
            input: ty.profile().clone(),
            result: ty.profile().clone(),
        }),
    );
    let copied = d.clone();
    drop(copied);
    h.add(
        &d,
        "native",
        0,
        method(|args: HookArgs<Rc<Marker>, Rc<Marker>>, _| {
            async move {
                let (value, next) = args.into_parts();
                let expected = value.clone();
                let result = next.unwrap().call(value).await?;
                assert!(Rc::ptr_eq(&expected, &result));
                Ok(result)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal(
        "native",
        ty.clone(),
        ty,
        method(|v: Rc<Marker>, _| async move { Ok(v) }.boxed_local()),
    );
    let m = h.manager.clone();
    let expected = Rc::new(Marker);
    let input = expected.clone();
    let result = h
        .run(move |ctx| m.run_dynamic_middleware(&d, input, &ctx, terminal))
        .unwrap();
    assert!(Rc::ptr_eq(&expected, &result));
    h.close();
}

#[test]
fn maximum_middleware_plan_uses_driver_trampoline_instead_of_recursive_poll_stack() {
    const COUNT: usize = 4096;
    let h = Harness::options(HookManagerOptions {
        max_contributions: COUNT,
        ..Default::default()
    });
    let d = middleware("wide");
    let definition = d.clone();
    let token = Rc::new(
        Contract::local(
            "wide",
            1,
            json!({}),
            BTreeMap::from([("run".into(), d.descriptor().contribution_member())]),
        )
        .unwrap(),
    );
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    h.host
        .serve(
            token,
            Rc::new(move |_, _, declaration| {
                for index in 0..COUNT {
                    definition.register(declaration, format!("item-{index:04}"), 0, "run")?;
                }
                let count = count.clone();
                let handler = hook_handler(method(move |args: HookArgs<i32, i32>, _| {
                    count.set(count.get() + 1);
                    async move {
                        let (v, next) = args.into_parts();
                        next.unwrap().call(v + 1).await
                    }
                    .boxed_local()
                }));
                Ok(
                    Implementation::new().method("run", move |args: HookArgs<i32, i32>, ctx| {
                        handler(args, ctx)
                    }),
                )
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();
    let terminal = h.terminal("wide", integer(), integer(), identity());
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_middleware(&d, 0, &ctx, terminal))
            .unwrap(),
        COUNT as i32
    );
    assert_eq!(calls.get(), COUNT);
    h.close();
}
