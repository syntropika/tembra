use futures::FutureExt;
use serde_json::json;
use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

fn token(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "value".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            )]),
        )
        .unwrap(),
    )
}
fn source_facet(
    token: Rc<Contract>,
    version: u64,
    saved: Rc<RefCell<Option<Cell>>>,
) -> ExecutableFacet {
    ExecutableFacet::foreign("source", vec![], move |_, setup| {
        let saved = saved.clone();
        setup.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let value = declaration.state(
                    "value",
                    StateDeclaration::new(json!({"type":"integer"}), json!(2))
                        .version(version)
                        .with_migrate(|_, _| Ok(json!(3))),
                )?;
                *saved.borrow_mut() = Some(value.clone());
                Ok(Implementation::new().state("value", value.readable()))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap()
}
fn dependent_case(proxy: bool, throws: bool, valid: bool) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("dependent-state", json!(null)),
        driver.clone(),
    );
    let source_token = token("source");
    let dependent_token = token("dependent");
    let saved = Rc::new(RefCell::new(None));
    driver
        .run_until(
            host.install_executable(
                source_facet(source_token.clone(), 1, saved.clone()),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let source = driver
        .run_until(host.get(source_token.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let setups = Rc::new(Counter::new(0));
    let factories = Rc::new(Counter::new(0));
    let (setup_count, factory_count, cell, dependency) = (
        setups.clone(),
        factories.clone(),
        saved.borrow().as_ref().unwrap().clone(),
        source_token.clone(),
    );
    let consumer = ExecutableFacet::foreign("dependent", vec![], move |_, setup| {
        setup_count.set(setup_count.get() + 1);
        let (cell, dependency, count) = (cell.clone(), dependency.clone(), factory_count.clone());
        setup.serve(
            dependent_token.clone(),
            Rc::new(move |_, _, declaration| {
                count.set(count.get() + 1);
                let input = if proxy {
                    declaration
                        .get(dependency.clone(), json!({}), GetOptions::default())?
                        .readable("value")?
                } else {
                    cell.readable()
                };
                let output =
                    declaration.derive(vec![input], json!({"type":"integer"}), move |values| {
                        if values[0].value() == &json!(3) {
                            if throws {
                                return Err(RuntimeError::invalid("dependent", "refused"));
                            }
                            if !valid {
                                return Ok(json!("invalid"));
                            }
                            return Ok(json!(30));
                        }
                        Ok(json!(values[0].value().as_i64().unwrap() * 10))
                    })?;
                Ok(Implementation::new().state("value", output))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    driver
        .run_until(
            host.install_executable(consumer, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let dependent = driver
        .run_until(host.get(token("dependent"), json!({}), GetOptions::default()))
        .unwrap();
    let leaf_factories = Rc::new(Counter::new(0));
    let leaf = if valid {
        let count = leaf_factories.clone();
        let leaf = ExecutableFacet::foreign("leaf", vec![], move |_, setup| {
            let count = count.clone();
            setup.serve(
                token("leaf"),
                Rc::new(move |_, _, declaration| {
                    count.set(count.get() + 1);
                    let dependency =
                        declaration.get(token("dependent"), json!({}), GetOptions::default())?;
                    let derived = declaration.derive(
                        vec![dependency.readable("value")?],
                        json!({"type":"integer"}),
                        |values| Ok(json!(values[0].value().as_i64().unwrap() * 2)),
                    )?;
                    Ok(Implementation::new().state("value", derived))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap();
        driver
            .run_until(
                host.install_executable(leaf, LoadOptions::default())
                    .unwrap(),
            )
            .unwrap();
        Some(
            driver
                .run_until(host.get(token("leaf"), json!({}), GetOptions::default()))
                .unwrap(),
        )
    } else {
        None
    };
    let a = source.readable("value").unwrap();
    let b = dependent.readable("value").unwrap();
    let before_a = a.snapshot().unwrap();
    let before_b = b.snapshot().unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    let sa = a
        .subscribe_events(move |event| {
            sink.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    let sink = seen.clone();
    let sb = b
        .subscribe_events(move |event| {
            sink.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    let atomic = Rc::new(RefCell::new(Vec::new()));
    let mut atomic_subscriptions = Vec::new();
    for port in [&a, &b] {
        let (a, b, host, atomic) = (a.clone(), b.clone(), host.clone(), atomic.clone());
        atomic_subscriptions.push(
            port.subscribe_events(move |event| {
                if matches!(event, StateEvent::Reset(_)) {
                    let epoch = host
                        .root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
                        .now_or_never()
                        .expect("publication permits an immediate managed root")?;
                    atomic.borrow_mut().push((
                        a.snapshot()?.value().clone(),
                        b.snapshot()?.value().clone(),
                        epoch,
                    ));
                }
                Ok(())
            })
            .unwrap(),
        );
    }
    let count = seen.borrow().len();
    let result = driver.run_until(host.reload(
        vec![source_facet(source_token, 2, saved.clone())],
        ReloadOptions::default(),
    ));
    if valid {
        assert_eq!(result.unwrap().current_version(), 2);
        assert_eq!(a.snapshot().unwrap().value(), &json!(3));
        assert_eq!(b.snapshot().unwrap().value(), &json!(30));
        assert_eq!(saved.borrow().as_ref().unwrap().version(), 2);
        assert!(seen.borrow().len() >= count + 2);
        assert!(atomic.borrow().len() >= 2);
        assert!(
            atomic
                .borrow()
                .iter()
                .all(|values| values == &(json!(3), json!(30), 2))
        );
        assert_eq!(
            leaf.as_ref()
                .unwrap()
                .readable("value")
                .unwrap()
                .snapshot()
                .unwrap()
                .value(),
            &json!(60)
        );
        assert_eq!(leaf_factories.get(), 1);
    } else {
        assert!(
            result.is_err(),
            "invalid prospective dependent must refuse the reload"
        );
        assert_eq!(a.snapshot().unwrap(), before_a);
        assert_eq!(b.snapshot().unwrap(), before_b);
        assert_eq!(
            seen.borrow().len(),
            count,
            "failed candidate must publish no events"
        );
        assert_eq!(saved.borrow().as_ref().unwrap().version(), 1);
    }
    assert_eq!(setups.get(), 1);
    assert_eq!(
        factories.get(),
        1,
        "unchanged dependent factory must not be replayed"
    );
    assert_eq!(
        driver
            .run_until(host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local()))
            .unwrap(),
        if valid { 2 } else { 1 }
    );
    for _ in 0..2 {
        driver
            .run_until(host.reload(vec![], ReloadOptions::default()))
            .unwrap();
    }
    let current = saved.borrow().as_ref().unwrap().clone();
    driver
        .run_until(host.root(move |context| {
            async move {
                current.replace(&context, json!(4), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(a.snapshot().unwrap().value(), &json!(4));
    assert_eq!(
        b.snapshot().unwrap().value(),
        &json!(40),
        "installed import subscriptions and constraints survive projection/abort"
    );
    assert_eq!(factories.get(), 1);
    if let Some(leaf) = &leaf {
        assert_eq!(
            leaf.readable("value").unwrap().snapshot().unwrap().value(),
            &json!(80)
        );
        assert_eq!(leaf_factories.get(), 1);
        leaf.release();
    }
    for subscription in atomic_subscriptions {
        subscription.unsubscribe();
    }
    sa.unsubscribe();
    sb.unsubscribe();
    dependent.release();
    source.release();
    driver.run_until(host.close()).unwrap();
}
#[test]
fn unchanged_direct_dependent_throw_refuses_migration_without_events() {
    dependent_case(false, true, false);
}
#[test]
fn unchanged_direct_dependent_invalid_value_refuses_migration_without_events() {
    dependent_case(false, false, false);
}
#[test]
fn unchanged_reference_proxy_throw_refuses_migration_without_events() {
    dependent_case(true, true, false);
}
#[test]
fn unchanged_reference_proxy_invalid_value_refuses_migration_without_events() {
    dependent_case(true, false, false);
}

#[test]
fn unchanged_direct_dependent_adopts_valid_projection_without_factory_replay() {
    dependent_case(false, false, true);
}
#[test]
fn unchanged_reference_proxy_adopts_valid_projection_without_factory_replay() {
    dependent_case(true, false, true);
}
