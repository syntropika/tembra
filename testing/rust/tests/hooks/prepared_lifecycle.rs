mod support;
use futures::FutureExt;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

#[test]
fn prepared_waterfall_follows_exact_implementation_alias_replacement_and_retirement() {
    use futures::channel::oneshot;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tembra_core::contracts::{Contract, Member, ValueSchema};

    let h = Harness::new();
    let token = Rc::new(
        Contract::local(
            "prepared.owner",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "run".into(),
                    Member::Method {
                        args: Some(ValueSchema::Native("prepared.integer@1".into())),
                        result: Some(ValueSchema::Native("prepared.integer@1".into())),
                    },
                ),
                (
                    "hold".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Native("prepared.integer@1".into())),
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    let handles = Rc::new(RefCell::new(Vec::<PreparedWaterfall<i32>>::new()));
    let captures = Rc::new(RefCell::new(Vec::<std::rc::Weak<()>>::new()));
    let constructions = Rc::new([Cell::new(0), Cell::new(0)]);
    let (release_call, waiting) = oneshot::channel();
    let waiting = Rc::new(RefCell::new(Some(waiting)));
    let (entered, entry) = oneshot::channel();
    let entered = Rc::new(RefCell::new(Some(entered)));
    let make_facet = {
        let (manager, token, handles, captures, constructions, waiting, entered) = (
            h.manager.clone(),
            token.clone(),
            handles.clone(),
            captures.clone(),
            constructions.clone(),
            waiting.clone(),
            entered.clone(),
        );
        move |generation: usize| {
            let (manager, token, handles, captures, constructions, waiting, entered) = (
                manager.clone(),
                token.clone(),
                handles.clone(),
                captures.clone(),
                constructions.clone(),
                waiting.clone(),
                entered.clone(),
            );
            ExecutableFacet::foreign("prepared-owner-facet", Vec::new(), move |_, setup| {
                let (manager, handles, captures, constructions, waiting, entered) = (
                    manager.clone(),
                    handles.clone(),
                    captures.clone(),
                    constructions.clone(),
                    waiting.clone(),
                    entered.clone(),
                );
                setup.serve(
                    token.clone(),
                    Rc::new(move |_, _, declaration| {
                        // The marker is created inside this exact factory pass. The facet
                        // definition, consumer reference and weak handle cannot retain it.
                        let capture = Rc::new(());
                        captures.borrow_mut().push(Rc::downgrade(&capture));
                        let constructions = constructions.clone();
                        let prepared = manager.declare_waterfall(
                            declaration,
                            "same-diagnostic-key",
                            move || {
                                constructions[generation].set(constructions[generation].get() + 1);
                                let input = HookType::portable(
                                    json!({
                                        "type":"integer",
                                        "minimum": if generation == 0 { -10 } else { 0 }
                                    }),
                                    move |value: &i32| {
                                        let _capture = &capture;
                                        Ok(json!(value))
                                    },
                                )?;
                                Ok(definition(
                                    "prepared.owner.hook",
                                    Strategy::Waterfall,
                                    input.clone(),
                                    input,
                                    None,
                                ))
                            },
                        )?;
                        handles.borrow_mut().push(prepared.clone());
                        let running = prepared.clone();
                        let (waiting, entered) = (waiting.clone(), entered.clone());
                        Ok(Implementation::new()
                            .method("run", move |value: i32, context| {
                                running.run(value, &context).boxed_local()
                            })
                            .method("hold", move |(): (), context| {
                                let waiting = waiting.borrow_mut().take().unwrap();
                                entered.borrow_mut().take().unwrap().send(()).unwrap();
                                let prepared = prepared.clone();
                                async move {
                                    waiting.await.unwrap();
                                    prepared.run(-1, &context).await
                                }
                                .boxed_local()
                            }))
                    }),
                    ServeOptions {
                        lifetime: Lifetime::Host,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
        }
    };

    h.driver
        .run_until(async {
            h.host
                .install_executable(make_facet(0), LoadOptions::default())?
                .await?;
            let reference = h.host.get(token, json!({}), GetOptions::default()).await?;
            assert_eq!(reference.call::<i32, i32>("run", -1, None).await?, -1);
            let old = handles.borrow()[0].clone();
            h.host.reload(Vec::new(), ReloadOptions::default()).await?;
            assert_eq!(reference.call::<i32, i32>("run", -2, None).await?, -2);
            assert_eq!(
                constructions[0].get(),
                1,
                "unchanged implementation aliases share state"
            );

            let (saved, previous) = oneshot::channel();
            let (finished_call, call_result) = oneshot::channel();
            let (end_root, ending_root) = oneshot::channel();
            let (host, held_reference) = (h.host.clone(), reference.clone());
            h.driver.spawn(
                async move {
                    host.root(move |context| {
                        async move {
                            saved.send(context.clone()).ok().unwrap();
                            let result = held_reference
                                .call::<(), i32>("hold", (), Some(&context))
                                .await;
                            finished_call.send(result).ok().unwrap();
                            let _ = ending_root.await;
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .await
                    .unwrap();
                }
                .boxed_local(),
            );
            let previous = previous.await.unwrap();
            entry.await.unwrap();
            assert_eq!(previous.code_version(), 2);
            h.host
                .reload(vec![make_facet(1)], ReloadOptions::default())
                .await?;
            assert_eq!(handles.borrow().len(), 2);
            assert_eq!(constructions[1].get(), 1, "replacement prepares eagerly");
            assert_eq!(reference.call::<i32, i32>("run", 0, None).await?, 0);
            assert_eq!(constructions[1].get(), 1);
            assert_eq!(
                reference
                    .call::<i32, i32>("run", -1, None)
                    .await
                    .unwrap_err()
                    .reason,
                "hook_value",
                "new implementation uses its changed descriptor"
            );
            let stale = old.clone();
            let error = h
                .host
                .root(move |context| stale.run(0, &context).boxed_local())
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Superseded);
            assert_eq!(constructions[0].get(), 1);
            assert_eq!(old.run(-3, &previous).await?, -3);
            assert!(captures.borrow()[0].upgrade().is_some());
            release_call.send(()).unwrap();
            assert_eq!(
                call_result.await.unwrap()?,
                -1,
                "entered old call finishes on its owner"
            );
            h.host
                .reload(
                    Vec::new(),
                    ReloadOptions {
                        retire_deadline: Duration::from_millis(1),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(
                old.run(0, &previous).await.unwrap_err().code,
                ErrorCode::Superseded
            );
            assert_eq!(constructions[0].get(), 1);
            assert!(
                captures.borrow()[0].upgrade().is_none(),
                "retired metadata releases captures"
            );
            end_root.send(()).unwrap();
            reference.release();
            h.host.close().await?;
            assert!(
                captures.borrow()[1].upgrade().is_none(),
                "close releases current metadata"
            );
            Ok::<(), RuntimeError>(())
        })
        .unwrap();
}

#[test]
fn failed_factory_invalid_implementation_and_refused_reload_release_prepared_metadata() {
    use serde_json::json;
    use std::collections::BTreeMap;
    use tembra_core::contracts::Contract;
    let h = Harness::new();
    let captures = Rc::new(RefCell::new(Vec::<std::rc::Weak<()>>::new()));
    for invalid in [false, true] {
        let token = Rc::new(
            Contract::local(format!("failed-{invalid}"), 1, json!({}), BTreeMap::new()).unwrap(),
        );
        let (manager, saved_captures) = (h.manager.clone(), captures.clone());
        h.host
            .serve(
                token.clone(),
                Rc::new(move |_, _, decl| {
                    let marker = Rc::new(());
                    saved_captures.borrow_mut().push(Rc::downgrade(&marker));
                    let _handle = manager.declare_waterfall(decl, "failed", || {
                        let input =
                            HookType::portable(json!({"type":"integer"}), move |v: &i32| {
                                let _ = &marker;
                                Ok(json!(v))
                            })?;
                        Ok(definition(
                            "failed",
                            Strategy::Waterfall,
                            input.clone(),
                            input,
                            None,
                        ))
                    })?;
                    if invalid {
                        Ok(Implementation::new()
                            .method("extra", |(): (), _| async { Ok(()) }.boxed_local()))
                    } else {
                        Err(RuntimeError::invalid(
                            "later",
                            "factory failed after successful preparation",
                        ))
                    }
                }),
                ServeOptions::default(),
            )
            .unwrap();
        assert!(
            h.driver
                .run_until(h.host.get(token, json!({}), GetOptions::default()))
                .is_err()
        );
        assert!(captures.borrow().last().unwrap().upgrade().is_none());
    }
    let token = Rc::new(Contract::local("reload-refusal", 1, json!({}), BTreeMap::new()).unwrap());
    let make_facet = {
        let (manager, captures, token) = (h.manager.clone(), captures.clone(), token.clone());
        move |reject: bool| {
            let (manager, captures, token) = (manager.clone(), captures.clone(), token.clone());
            ExecutableFacet::foreign("reload-refusal", vec![], move |_, setup| {
                let (manager, captures) = (manager.clone(), captures.clone());
                setup.serve(
                    token.clone(),
                    Rc::new(move |_, _, decl| {
                        let marker = Rc::new(());
                        captures.borrow_mut().push(Rc::downgrade(&marker));
                        let _handle = manager.declare_waterfall(decl, "candidate", || {
                            let input =
                                HookType::portable(json!({"type":"integer"}), move |v: &i32| {
                                    let _ = &marker;
                                    Ok(json!(v))
                                })?;
                            Ok(definition(
                                "candidate",
                                Strategy::Waterfall,
                                input.clone(),
                                input,
                                None,
                            ))
                        })?;
                        if reject {
                            Err(RuntimeError::invalid("candidate", "reject"))
                        } else {
                            Ok(Implementation::new())
                        }
                    }),
                    ServeOptions {
                        lifetime: Lifetime::Host,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
        }
    };
    h.driver
        .run_until(
            h.host
                .install_executable(make_facet(false), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let reference = h
        .driver
        .run_until(h.host.get(token, json!({}), GetOptions::default()))
        .unwrap();
    assert!(captures.borrow()[2].upgrade().is_some());
    assert!(
        h.driver
            .run_until(
                h.host
                    .reload(vec![make_facet(true)], ReloadOptions::default())
            )
            .is_err()
    );
    assert!(
        captures.borrow()[2].upgrade().is_some(),
        "current implementation survives candidate refusal"
    );
    assert!(
        captures.borrow()[3].upgrade().is_none(),
        "rejected candidate metadata released"
    );
    reference.release();
    h.close();
    assert!(
        captures
            .borrow()
            .iter()
            .all(|marker| marker.upgrade().is_none())
    );
}
