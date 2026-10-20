use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use serde_json::{Value, json};
use tembra_core::{
    Cell, GetOptions, Host, HostOptions, Implementation, ManualDriver, ReplaceOptions,
    RuntimeError, ServeOptions, StateDeclaration,
    contracts::{Contract, Member, ValueSchema},
};

#[test]
fn reference_observers_see_complete_derivations_and_reentrant_fifo_commits() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let integer = json!({"type":"integer","minimum":0,"maximum":100});
        let contract = Rc::new(
            Contract::local(
                "acceptance.state",
                1,
                json!({}),
                BTreeMap::from([
                    (
                        "count".into(),
                        Member::State {
                            schema: integer.clone(),
                        },
                    ),
                    (
                        "double".into(),
                        Member::State {
                            schema: integer.clone(),
                        },
                    ),
                    (
                        "increment".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Portable(integer.clone())),
                        },
                    ),
                ]),
            )
            .unwrap(),
        );
        let host = Host::new(HostOptions::new("state", json!("host")), driver.clone());
        let saved = Rc::new(RefCell::new(None::<Cell>));
        let captured = saved.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let count = declaration
                    .state("count", StateDeclaration::new(integer.clone(), json!(0.0)))?;
                let double = declaration.derive(
                    vec![count.readable()],
                    json!({"type":"integer","minimum":0,"maximum":4}),
                    |sources| Ok(json!(sources[0].value().as_i64().unwrap() * 2)),
                )?;
                *captured.borrow_mut() = Some(count.clone());
                Ok(Implementation::new()
                    .state("count", count.readable())
                    .state("double", double)
                    .json_method("increment", move |_, ctx| {
                        let count = count.clone();
                        async move {
                            let committed = count.change(&ctx, |draft| {
                                *draft = json!(draft.as_i64().unwrap() + 1);
                                Ok(())
                            })?;
                            Ok(Some(committed.value().clone()))
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = host
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        let count = reference.readable("count").unwrap();
        let double = reference.readable("double").unwrap();
        let cell = saved.borrow().as_ref().unwrap().clone();
        let retained = count.snapshot().unwrap();
        let observations = Rc::new(RefCell::new(Vec::new()));
        host.root(|ctx| {
            let reference = reference.clone();
            let count = count.clone();
            let double = double.clone();
            let cell = cell.clone();
            let observations = observations.clone();
            async move {
                let peer = double.clone();
                let writer = cell.clone();
                let write_context = ctx.clone();
                let log = observations.clone();
                let first = count.subscribe(move |snapshot| {
                    let value = snapshot.value().as_i64().unwrap();
                    log.borrow_mut().push((
                        "count",
                        value,
                        peer.snapshot()?.value().as_i64().unwrap(),
                    ));
                    if value == 1 {
                        writer.replace(&write_context, json!(2), ReplaceOptions::default())?;
                    }
                    Ok(())
                })?;
                let peer = count.clone();
                let log = observations.clone();
                let second = double.subscribe(move |snapshot| {
                    log.borrow_mut().push((
                        "double",
                        snapshot.value().as_i64().unwrap(),
                        peer.snapshot()?.value().as_i64().unwrap(),
                    ));
                    Ok(())
                })?;
                observations.borrow_mut().clear();
                assert_eq!(
                    reference.call_json("increment", None, Some(&ctx)).await?,
                    Some(json!(1))
                );
                assert_eq!(
                    *observations.borrow(),
                    [
                        ("count", 1, 2),
                        ("double", 2, 1),
                        ("count", 2, 4),
                        ("double", 4, 2)
                    ]
                );
                let before = (count.snapshot()?, double.snapshot()?);
                let delivered = observations.borrow().len();
                // The cell schema accepts 3, but its internal derivation rejects 6.
                assert!(
                    cell.replace(&ctx, json!(3), ReplaceOptions::default())
                        .is_err()
                );
                assert_eq!(count.snapshot()?.value(), before.0.value());
                assert_eq!(count.snapshot()?.revision(), before.0.revision());
                assert_eq!(double.snapshot()?.value(), before.1.value());
                assert_eq!(double.snapshot()?.revision(), before.1.revision());
                assert_eq!(observations.borrow().len(), delivered);
                cell.replace(&ctx, json!(2.0), ReplaceOptions::default())?;
                assert_eq!(observations.borrow().len(), delivered);
                assert!(
                    cell.change(&ctx, |draft| {
                        *draft = json!(99);
                        Err(RuntimeError::invalid(
                            "application",
                            "discard this private draft",
                        ))
                    })
                    .is_err()
                );
                assert_eq!(count.snapshot()?.value(), &json!(2));
                first.unsubscribe();
                second.unsubscribe();
                Ok(())
            }
            .boxed_local()
        })
        .await
        .unwrap();
        assert_eq!(retained.value(), &json!(0));
        let mut copied = retained.shared_value();
        *Rc::make_mut(&mut copied) = json!(99);
        assert_eq!(retained.value(), &json!(0));
        reference.release();
        host.close().await.unwrap();
        assert!(count.snapshot().is_err());
        assert!(double.snapshot().is_err());
        assert!(cell.snapshot().is_err());
        assert_eq!(retained.value(), &Value::from(0));
    });
}

#[test]
fn retained_preparations_cannot_cross_scopes_or_survive_an_intervening_noop() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let schema = json!({"type":"integer"});
        let contract = Rc::new(
            Contract::local(
                "acceptance.prepared",
                1,
                json!({}),
                BTreeMap::from([(
                    "value".into(),
                    Member::State {
                        schema: schema.clone(),
                    },
                )]),
            )
            .unwrap(),
        );
        let host = Host::new(HostOptions::new("state", json!("host")), driver.clone());
        let saved = Rc::new(RefCell::new(None::<Cell>));
        let captured = saved.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let cell =
                    declaration.state("value", StateDeclaration::new(schema.clone(), json!(0)))?;
                *captured.borrow_mut() = Some(cell.clone());
                Ok(Implementation::new().state("value", cell.readable()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = host
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        let cell = saved.borrow().as_ref().unwrap().clone();
        let writer = cell.clone();
        let escaped = host
            .root(move |ctx| {
                async move { writer.prepare_replace(&ctx, json!(1), ReplaceOptions::default()) }
                    .boxed_local()
            })
            .await
            .unwrap();
        host.root(move |ctx| {
            async move {
                assert!(cell.adopt(&ctx, &escaped).is_err());
                assert_eq!(cell.snapshot()?.value(), &json!(0));
                let stale = cell.prepare_replace(&ctx, json!(2), ReplaceOptions::default())?;
                let before = cell.snapshot()?;
                cell.replace(&ctx, json!(0.0), ReplaceOptions::default())?;
                assert!(cell.adopt(&ctx, &stale).is_err());
                assert_eq!(cell.snapshot()?.revision(), before.revision());
                let aborted = cell.prepare_replace(&ctx, json!(3), ReplaceOptions::default())?;
                aborted.abort();
                aborted.abort();
                assert!(cell.adopt(&ctx, &aborted).is_err());
                assert_eq!(cell.snapshot()?.value(), &json!(0));
                let accepted = cell.prepare_replace(&ctx, json!(4), ReplaceOptions::default())?;
                assert_eq!(cell.adopt(&ctx, &accepted)?.value(), &json!(4));
                assert!(cell.adopt(&ctx, &accepted).is_err());
                assert_eq!(cell.snapshot()?.value(), &json!(4));
                Ok(())
            }
            .boxed_local()
        })
        .await
        .unwrap();
        reference.release();
        host.close().await.unwrap();
    });
}
