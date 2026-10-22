mod support;
use futures::FutureExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

#[test]
fn cloned_metadata_survives_authoring_drop_without_retaining_execution_admission() {
    let h = Harness::new();
    let definition = {
        let authored = middleware("shared-owner");
        authored.clone()
    };
    let entries = Rc::new(std::cell::Cell::new(0));
    let counter = entries.clone();
    let terminal = {
        let authored = h.terminal(
            "shared-owner",
            integer(),
            integer(),
            method(move |value, _| {
                counter.set(counter.get() + 1);
                async move { Ok(value) }.boxed_local()
            }),
        );
        authored.clone()
    };
    let (manager, copied_definition, copied_terminal) =
        (h.manager.clone(), definition.clone(), terminal.clone());
    let expired = h
        .driver
        .run_until(h.host.root(move |context| {
            async move {
                assert_eq!(
                    manager
                        .run_dynamic_middleware(&copied_definition, 3, &context, copied_terminal)
                        .await?,
                    3
                );
                Ok(context)
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(entries.get(), 1);
    assert_eq!(
        h.driver
            .run_until(
                h.manager
                    .run_dynamic_middleware(&definition, 4, &expired, terminal.clone())
            )
            .unwrap_err()
            .reason,
        "expired"
    );
    h.close();
    assert!(
        h.driver
            .run_until(
                h.manager
                    .run_dynamic_middleware(&definition, 5, &expired, terminal)
            )
            .is_err()
    );
    assert_eq!(entries.get(), 1);
}

#[test]
fn retained_terminal_profiles_reject_disagreement_and_check_each_current_value() {
    let h = Harness::new();
    let bounded = || HookType::<i32>::serde(json!({"type":"integer", "minimum":0})).unwrap();
    let input = bounded();
    let d = definition(
        "retained-agreement",
        Strategy::Middleware,
        input.clone(),
        input.clone(),
        Some(TerminalProfile {
            input: input.profile().clone(),
            result: input.profile().clone(),
        }),
    );
    let entries = Rc::new(std::cell::Cell::new(0));
    let counter = entries.clone();
    let terminal = h.terminal(
        "retained-matching",
        bounded(),
        bounded(),
        method(move |value, _| {
            counter.set(counter.get() + 1);
            async move { Ok(value) }.boxed_local()
        }),
    );
    let stricter = HookType::<i32>::serde(json!({"type":"integer", "minimum":10})).unwrap();
    let counter = entries.clone();
    let disagreeing = h.terminal(
        "retained-disagreeing",
        stricter.clone(),
        stricter,
        method(move |value, _| {
            counter.set(counter.get() + 1);
            async move { Ok(value) }.boxed_local()
        }),
    );
    let (manager, definition) = (h.manager.clone(), d.clone());
    assert_eq!(
        h.run(move |ctx| manager.run_dynamic_middleware(&definition, 20, &ctx, disagreeing))
            .unwrap_err()
            .reason,
        "hook_signature"
    );
    assert_eq!(entries.get(), 0);
    for value in [3, -1, 4] {
        let (manager, definition, terminal) = (h.manager.clone(), d.clone(), terminal.clone());
        let result =
            h.run(move |ctx| manager.run_dynamic_middleware(&definition, value, &ctx, terminal));
        if value >= 0 {
            assert_eq!(result.unwrap(), value);
        } else {
            assert_eq!(result.unwrap_err().reason, "hook_value");
        }
    }
    assert_eq!(entries.get(), 2);
    h.close();
}

#[test]
fn five_empty_outcomes_and_terminal_presence_are_explicit() {
    let h = Harness::new();
    let series = definition(
        "series",
        Strategy::Series,
        integer(),
        HookType::absent(),
        None,
    );
    let bail = definition("bail", Strategy::Bail, integer(), integer(), None);
    let waterfall = definition("waterfall", Strategy::Waterfall, integer(), integer(), None);
    let parallel = definition("parallel", Strategy::Parallel, integer(), integer(), None);
    let mw = middleware("middleware");
    let terminal = h.terminal("identity", integer(), integer(), identity());
    let m = h.manager.clone();
    h.run(move |ctx| m.run_dynamic_series(&series, 3, &ctx))
        .unwrap();
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_bail(&bail, 3, &ctx, None))
            .unwrap(),
        BailOutcome::Unresolved
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_waterfall(&waterfall, 3, &ctx))
            .unwrap(),
        3
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_parallel(&parallel, 3, &ctx))
            .unwrap(),
        Vec::<i32>::new()
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_middleware(&mw, 3, &ctx, terminal))
            .unwrap(),
        3
    );
    h.close();
}
#[test]
fn priority_order_waterfall_parallel_and_all_terminal_forms() {
    let h = Harness::new();
    let order = Rc::new(RefCell::new(Vec::new()));
    let terminal_profile = Some(TerminalProfile {
        input: integer().profile().clone(),
        result: integer().profile().clone(),
    });
    let series = definition(
        "series",
        Strategy::Series,
        integer(),
        HookType::absent(),
        terminal_profile.clone(),
    );
    for (name, priority) in [("later", 4), ("first", -1), ("tie", -1)] {
        let order = order.clone();
        h.add(
            &series,
            name,
            priority,
            method(move |args: HookArgs<i32, ()>, _| {
                assert_eq!(*args.input(), 7);
                order.borrow_mut().push(name);
                async { Ok(()) }.boxed_local()
            }),
        );
    }
    let terminal = h.terminal("series", integer(), integer(), identity());
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_series_then(&series, 7, &ctx, terminal))
            .unwrap(),
        7
    );
    assert_eq!(&*order.borrow(), &["first", "tie", "later"]);
    let waterfall = definition(
        "waterfall",
        Strategy::Waterfall,
        integer(),
        integer(),
        terminal_profile,
    );
    for (key, factor) in [("w1", 2), ("w2", 3)] {
        h.add(
            &waterfall,
            key,
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move { Ok(args.into_parts().0 * factor) }.boxed_local()
            }),
        );
    }
    let terminal = h.terminal(
        "waterfall",
        integer(),
        integer(),
        method(|value, _| async move { Ok(value + 1) }.boxed_local()),
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_waterfall_then(&waterfall, 2, &ctx, terminal))
            .unwrap(),
        13
    );
    let vector =
        HookType::<Vec<i32>>::serde(json!({"type":"array","items":{"type":"integer"}})).unwrap();
    let parallel = definition(
        "parallel",
        Strategy::Parallel,
        integer(),
        integer(),
        Some(TerminalProfile {
            input: vector.profile().clone(),
            result: integer().profile().clone(),
        }),
    );
    for (key, amount) in [("p1", 3), ("p2", 5)] {
        h.add(
            &parallel,
            key,
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move { Ok(args.into_parts().0 + amount) }.boxed_local()
            }),
        );
    }
    let terminal = h.terminal(
        "parallel",
        vector,
        integer(),
        method(|values: Vec<i32>, _| async move { Ok(values.into_iter().sum()) }.boxed_local()),
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_parallel_then(&parallel, 10, &ctx, terminal))
            .unwrap(),
        28
    );
    h.close();
}
#[test]
fn bail_false_and_null_are_stops_and_terminal_runs_only_after_continue() {
    for payload in [json!(false), json!(null)] {
        let h = Harness::new();
        let result =
            HookType::<Value>::serde(json!({"anyOf":[{"type":"boolean"},{"type":"null"}]}))
                .unwrap();
        let d = definition("choose", Strategy::Bail, HookType::absent(), result, None);
        let expected = payload.clone();
        h.add(
            &d,
            "stop",
            0,
            method(move |_: HookArgs<(), BailDecision<Value>>, _| {
                let payload = payload.clone();
                async move { Ok(BailDecision::Stop(payload)) }.boxed_local()
            }),
        );
        h.ready();
        let m = h.manager.clone();
        assert_eq!(
            h.run(move |ctx| m.run_dynamic_bail(&d, (), &ctx, None))
                .unwrap(),
            BailOutcome::Resolved(expected)
        );
        h.close();
    }
    let h = Harness::new();
    let d = definition(
        "bail-terminal",
        Strategy::Bail,
        integer(),
        integer(),
        Some(TerminalProfile {
            input: integer().profile().clone(),
            result: integer().profile().clone(),
        }),
    );
    h.add(
        &d,
        "continue",
        0,
        method(|_: HookArgs<i32, BailDecision<i32>>, _| {
            async { Ok(BailDecision::Continue) }.boxed_local()
        }),
    );
    let terminal = h.terminal("bail", integer(), integer(), identity());
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_bail(&d, 9, &ctx, Some(terminal)))
            .unwrap(),
        BailOutcome::Resolved(9)
    );
    h.close();
}
#[test]
fn middleware_modifies_short_circuits_and_requires_named_recovery() {
    for mode in [0, 1, 2, 3] {
        let h = Harness::new();
        let d = middleware("middleware");
        h.add(
            &d,
            "outer",
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move {
                    let (input, next) = args.into_parts();
                    let next = next.unwrap();
                    if mode == 1 {
                        return Ok(55);
                    }
                    let result = next.call(input + 1).await;
                    if mode >= 2 {
                        assert!(result.is_err());
                        if mode == 3 {
                            return next.recover(42);
                        }
                        return Ok(42);
                    }
                    Ok(result? + 10)
                }
                .boxed_local()
            }),
        );
        let terminal = h.terminal(
            "terminal",
            integer(),
            integer(),
            method(move |value, _| {
                async move {
                    if mode >= 2 {
                        Err(RuntimeError::invalid("terminal_failed", "terminal failed"))
                    } else {
                        Ok(value * 2)
                    }
                }
                .boxed_local()
            }),
        );
        let m = h.manager.clone();
        let result = h.run(move |ctx| m.run_dynamic_middleware(&d, 3, &ctx, terminal));
        match mode {
            0 => assert_eq!(result.unwrap(), 18),
            1 => assert_eq!(result.unwrap(), 55),
            2 => assert_eq!(result.unwrap_err().reason, "terminal_failed"),
            3 => assert_eq!(result.unwrap(), 42),
            _ => unreachable!(),
        };
        h.close();
    }
}
