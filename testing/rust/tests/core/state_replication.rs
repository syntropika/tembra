use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use futures::FutureExt;
use serde_json::{Value, json};
use tembra_core::{
    Cell, GetOptions, Host, HostOptions, Implementation, ManualDriver, ReplaceOptions,
    RuntimeError, ServeOptions, StateDeclaration, StateEvent, Subscription,
    contracts::{Contract, Member, canonical_json},
    delta::{DeltaBatch, DeltaOp, apply},
};

fn operations(event: &StateEvent) -> &DeltaBatch {
    match event {
        StateEvent::Ops { delta, .. } => delta,
        other => panic!("expected public reference operations, received {other:?}"),
    }
}

#[test]
fn public_reference_events_replay_exact_revisions_and_keep_old_snapshots_immutable() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let initial = json!({
            "object": {"drop": true, "keep": null},
            "items": [{"n": 1}, "😀", null],
            "text": "e\u{301}"
        });
        let contract = Rc::new(
            Contract::local(
                "acceptance.replication",
                1,
                json!({}),
                BTreeMap::from([("document".into(), Member::State { schema: json!({}) })]),
            )
            .unwrap(),
        );
        let host = Host::new(
            HostOptions::new("replication", json!("host")),
            driver.clone(),
        );
        let cells = Rc::new(RefCell::new(None::<(Cell, Cell)>));
        let captured = cells.clone();
        let initial_for_factory = initial.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let document = declaration.state(
                    "document",
                    StateDeclaration::new(json!({}), initial_for_factory.clone()),
                )?;
                let private = declaration.state(
                    "private",
                    StateDeclaration::new(json!({ "type": "integer" }), json!(0)),
                )?;
                *captured.borrow_mut() = Some((document.clone(), private));
                Ok(Implementation::new().state("document", document.readable()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = host
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        let readable = reference.readable("document").unwrap();
        assert!(reference.readable("private").is_err());
        let (document, private) = cells.borrow().as_ref().unwrap().clone();
        let retained = readable.snapshot().unwrap();
        let replayed = Rc::new(RefCell::new(None::<(Value, u64)>));
        let events = Rc::new(RefCell::new(Vec::<StateEvent>::new()));
        let received = events.clone();
        let replica = replayed.clone();
        let subscription = readable
            .subscribe_events(move |event| {
                let snapshot = event.snapshot();
                let candidate = match &event {
                    StateEvent::Snapshot(snapshot) => {
                        assert!(
                            replica.borrow().is_none(),
                            "only the first event is a snapshot"
                        );
                        snapshot.value().clone()
                    }
                    StateEvent::Ops { delta, snapshot } => {
                        let previous = replica.borrow();
                        let (value, revision) = previous.as_ref().unwrap();
                        assert_eq!(delta.base_revision(), *revision);
                        assert_eq!(delta.result_revision(), snapshot.revision());
                        apply(value, *revision, delta).map_err(|error| {
                            RuntimeError::invalid("replication", error.to_string())
                        })?
                    }
                    StateEvent::Reset(_) => panic!("local writes must retain their operations"),
                };
                assert_eq!(
                    canonical_json(&candidate).unwrap(),
                    canonical_json(snapshot.value()).unwrap()
                );
                *replica.borrow_mut() = Some((candidate, snapshot.revision()));
                received.borrow_mut().push(event);
                Ok(())
            })
            .unwrap();
        assert_eq!(events.borrow().len(), 1);
        assert!(matches!(&events.borrow()[0], StateEvent::Snapshot(_)));

        let victim = Rc::new(RefCell::new(None::<Subscription>));
        let pending_victim = victim.clone();
        let remover = readable
            .subscribe_events(move |event| {
                if matches!(event, StateEvent::Ops { .. }) {
                    let removed = pending_victim.borrow_mut().take();
                    if let Some(subscription) = removed {
                        subscription.unsubscribe();
                    }
                }
                Ok(())
            })
            .unwrap();
        let victim_calls = Rc::new(Counter::new(0));
        let counted = victim_calls.clone();
        *victim.borrow_mut() = Some(
            readable
                .subscribe_events(move |_| {
                    counted.set(counted.get() + 1);
                    Ok(())
                })
                .unwrap(),
        );
        assert_eq!(victim_calls.get(), 1);

        let observed = events.clone();
        host.root(move |ctx| {
            async move {
                document.change(&ctx, |draft| {
                    draft["object"].as_object_mut().unwrap().remove("drop");
                    draft["object"]["new"] = Value::Null;
                    let items = draft["items"].as_array_mut().unwrap();
                    items.remove(1);
                    items.push(json!("終😀"));
                    draft["text"] = json!("é");
                    Ok(())
                })?;
                assert_eq!(observed.borrow().len(), 2);
                assert!(!operations(&observed.borrow()[1]).ops().is_empty());

                let first_revision = document.snapshot()?.revision();
                private.replace(&ctx, json!(1), ReplaceOptions::default())?;
                assert_eq!(document.snapshot()?.revision(), first_revision);
                assert_eq!(observed.borrow().len(), 2);
                document.change(&ctx, |draft| {
                    draft["items"]
                        .as_array_mut()
                        .unwrap()
                        .splice(0..1, [json!({"n": 2}), json!("λ")]);
                    draft["object"].as_object_mut().unwrap().remove("new");
                    Ok(())
                })?;
                let second_revision = document.snapshot()?.revision();
                assert!(
                    second_revision > first_revision + 1,
                    "private commits skip this node's epochs"
                );
                let skipped = observed.borrow();
                let delta = operations(&skipped[2]);
                assert_eq!(delta.base_revision(), first_revision);
                assert_eq!(delta.result_revision(), second_revision);
                drop(skipped);

                let replacement = json!({"replacement": [null, "😀"], "n": 1, "zero": 0});
                document.replace(&ctx, replacement.clone(), ReplaceOptions::default())?;
                assert_eq!(
                    operations(&observed.borrow()[3]).ops(),
                    &[DeltaOp::Set {
                        path: vec![],
                        value: replacement,
                    }]
                );

                let structural =
                    json!({"replacement": [null, "😀", {"new": true}], "n": 2, "zero": 0});
                document.replace(&ctx, structural, ReplaceOptions { diff: true })?;
                let structural_event = observed.borrow();
                let ops = operations(&structural_event[4]).ops();
                assert!(!ops.is_empty());
                assert!(
                    ops.iter()
                        .all(|op| !matches!(op, DeltaOp::Set { path, .. } if path.is_empty()))
                );
                drop(structural_event);

                let last = document.snapshot()?;
                let delivered = observed.borrow().len();
                let mut semantic_noop = last.value().clone();
                semantic_noop["n"] = json!(2.0);
                semantic_noop["zero"] = json!(-0.0);
                document.replace(&ctx, semantic_noop, ReplaceOptions { diff: true })?;
                assert_eq!(document.snapshot()?.revision(), last.revision());
                assert_eq!(observed.borrow().len(), delivered);
                Ok(())
            }
            .boxed_local()
        })
        .await
        .unwrap();

        assert_eq!(events.borrow().len(), 5);
        assert_eq!(
            victim_calls.get(),
            1,
            "a removed observer is skipped within the same delivery"
        );
        assert_eq!(retained.value(), &initial);
        assert_eq!(
            replayed.borrow().as_ref().unwrap().0,
            *readable.snapshot().unwrap().value()
        );
        subscription.unsubscribe();
        remover.unsubscribe();
        reference.release();
        host.close().await.unwrap();
    });
}
