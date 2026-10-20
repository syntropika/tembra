//! Dependency bindings retain the epoch that authorized their declaration.
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

type Gate = Rc<RefCell<Option<oneshot::Receiver<()>>>>;

fn contract(name: &str, version: u64) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            version,
            json!({}),
            BTreeMap::from([
                (
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                ),
                (
                    "wait".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
            ]),
        )
        .unwrap(),
    )
}

fn provider(token: Rc<Contract>, value: u64) -> ExecutableFacet {
    ExecutableFacet::foreign("provider", vec![], move |_, setup| {
        setup.serve(
            token.clone(),
            Rc::new(move |_, _, _| {
                Ok(Implementation::new()
                    .json_method("read", move |_, _| {
                        async move { Ok(Some(json!(value))) }.boxed_local()
                    })
                    .json_method("wait", |_, _| async { Ok(None) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
    })
    .unwrap()
}

fn consumer(
    token: Rc<Contract>,
    provider: Rc<Contract>,
    pending_call: Option<Gate>,
    pending_open: Option<Gate>,
) -> ExecutableFacet {
    ExecutableFacet::foreign("consumer", vec![provider.clone()], move |_, setup| {
        let (provider, pending_call, pending_open) =
            (provider.clone(), pending_call.clone(), pending_open.clone());
        setup.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency =
                    declaration.get(provider.clone(), json!({}), GetOptions::default())?;
                if let Some(gate) = pending_open.clone() {
                    declaration.resource(
                        "candidate-open",
                        ResourceDeclaration::new(move |_| {
                            let wait = gate
                                .borrow_mut()
                                .take()
                                .expect("one candidate resource open");
                            async move {
                                wait.await.unwrap();
                                Ok(())
                            }
                            .boxed_local()
                        }),
                    )?;
                }
                let call_gate = pending_call.clone();
                Ok(Implementation::new()
                    .json_method("read", move |_, context| {
                        let dependency = dependency.clone();
                        async move { dependency.call_json("read", None, Some(&context)).await }
                            .boxed_local()
                    })
                    .json_method("wait", move |_, _| {
                        let wait = call_gate
                            .as_ref()
                            .expect("old pending invocation")
                            .borrow_mut()
                            .take()
                            .unwrap();
                        async move {
                            wait.await.unwrap();
                            Ok(None)
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

fn fixture(pending_call: Option<Gate>) -> (Host, Rc<ManualDriver>, Rc<Contract>) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("dependency-epochs", Value::Null),
        driver.clone(),
    );
    let service = contract("consumer", 1);
    let old_provider = contract("provider", 1);
    driver
        .run_until(
            host.load_executable(
                vec![
                    provider(old_provider.clone(), 1),
                    consumer(service.clone(), old_provider, pending_call, None),
                ],
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    (host, driver, service)
}

#[test]
fn an_old_declaration_dependency_does_not_poison_a_later_empty_reload() {
    let (release, waiting) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(waiting)));
    let (host, driver, service) = fixture(Some(gate.clone()));
    let reference = driver
        .run_until(host.get(service.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let running = reference.clone();
    let (complete, completion) = oneshot::channel();
    driver.spawn(
        async move {
            let _ = complete.send(running.call_json("wait", None, None).await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        gate.borrow().is_none(),
        "physical V1 callback entered its actual wait"
    );
    let current = contract("provider", 2);
    driver
        .run_until(host.reload(
            vec![
                consumer(service, current.clone(), None, None),
                provider(current, 2),
            ],
            ReloadOptions::default(),
        ))
        .unwrap();
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(2))
    );
    let third = driver.run_until(host.reload(
        vec![],
        ReloadOptions {
            retire_deadline: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    release.send(()).unwrap();
    let _ = driver.run_until(completion).unwrap();
    let report = third.expect("old declaration-only bindings cannot fail current proxy adoption");
    assert_eq!(report.current_version(), 3);
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(2))
    );
    reference.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn a_candidate_dependency_survives_a_real_resource_open_await_before_commit() {
    let (host, driver, service) = fixture(None);
    let reference = driver
        .run_until(host.get(service.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let current = contract("provider", 2);
    let (release, waiting) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(waiting)));
    let reload = host.reload(
        vec![
            consumer(service, current.clone(), None, Some(gate.clone())),
            provider(current, 2),
        ],
        ReloadOptions::default(),
    );
    let (complete, completion) = oneshot::channel();
    driver.spawn(
        async move {
            let _ = complete.send(reload.await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        gate.borrow().is_none(),
        "candidate open actually suspended before adoption"
    );
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(1))
    );
    release.send(()).unwrap();
    let report = driver.run_until(completion).unwrap().unwrap();
    assert_eq!(report.current_version(), 2);
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(2))
    );
    reference.release();
    driver.run_until(host.close()).unwrap();
}
