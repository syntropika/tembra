//! Cleanup roots use the current installed epoch and never resurrect old provider code.
use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

fn provider_token(version: u64) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "cleanup-provider",
            version,
            json!({}),
            BTreeMap::from([
                (
                    "flush".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
                (
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                ),
            ]),
        )
        .unwrap(),
    )
}
fn consumer_token() -> Rc<Contract> {
    Rc::new(Contract::local("cleanup-consumer", 1, json!({}), BTreeMap::new()).unwrap())
}
fn provider(version: u64, effects: Rc<RefCell<Vec<u64>>>) -> ExecutableFacet {
    ExecutableFacet::foreign("provider", vec![], move |_, setup| {
        let effects = effects.clone();
        setup.serve(
            provider_token(version),
            Rc::new(move |_, _, _| {
                let effects = effects.clone();
                Ok(Implementation::new()
                    .method("flush", move |(): (), context| {
                        effects.borrow_mut().push(context.code_version());
                        async { Ok(()) }.boxed_local()
                    })
                    .json_method("read", move |_, _| {
                        async move { Ok(Some(json!(version))) }.boxed_local()
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
type Outcomes = Rc<RefCell<Vec<(u64, Result<()>)>>>;
fn consumer(version: u64, outcomes: Outcomes, closed: Rc<RefCell<Vec<u64>>>) -> ExecutableFacet {
    ExecutableFacet::foreign("consumer", vec![], move |scope, setup| {
        let (scope, outcomes, closed) = (scope.clone(), outcomes.clone(), closed.clone());
        setup.serve(
            consumer_token(),
            Rc::new(move |_, _, declaration| {
                let (scope, outcomes, closed) = (scope.clone(), outcomes.clone(), closed.clone());
                let dependency = if version == 1 {
                    Some(declaration.get(provider_token(1), json!({}), GetOptions::default())?)
                } else {
                    None
                };
                let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                    .with_close(move |_| {
                        let (scope, outcomes, closed, dependency) = (
                            scope.clone(),
                            outcomes.clone(),
                            closed.clone(),
                            dependency.clone(),
                        );
                        async move {
                            closed.borrow_mut().push(version);
                            if let Some(dependency) = dependency {
                                scope
                                    .root(move |context| {
                                        async move {
                                            let epoch = context.code_version();
                                            let result = dependency
                                                .call::<(), ()>("flush", (), Some(&context))
                                                .await;
                                            outcomes.borrow_mut().push((epoch, result.clone()));
                                            result
                                        }
                                        .boxed_local()
                                    })
                                    .await
                            } else {
                                Ok(())
                            }
                        }
                        .boxed_local()
                    });
                if version == 1 {
                    resource
                        .deps
                        .push(ResourceDependency::Reference(declaration.get(
                            provider_token(1),
                            json!({}),
                            GetOptions::default(),
                        )?));
                }
                declaration.resource("cleanup", resource)?;
                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap()
}
#[test]
fn removing_a_consumer_does_not_exempt_its_actual_cleanup_dependency_from_coverage() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("cleanup-removal", json!(null)),
        driver.clone(),
    );
    let effects = Rc::new(RefCell::new(Vec::new()));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let closed = Rc::new(RefCell::new(Vec::new()));
    driver
        .run_until(
            host.load_executable(
                vec![
                    provider(1, effects.clone()),
                    consumer(1, outcomes.clone(), closed.clone()),
                ],
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let reference = driver
        .run_until(host.get(consumer_token(), json!({}), GetOptions::default()))
        .unwrap();
    let error = driver
        .run_until(host.reload(
            vec![provider(2, effects.clone())],
            ReloadOptions {
                remove: vec!["consumer".into()],
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ContractMismatch);
    assert_eq!(error.reason, "version");
    assert_eq!(reference.status(), RefStatus::Live);
    assert!(closed.borrow().is_empty());
    assert!(effects.borrow().is_empty());
    assert_eq!(
        driver
            .run_until(host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local()))
            .unwrap(),
        1
    );
    reference.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(&*effects.borrow(), &[1]);
    assert_eq!(outcomes.borrow().len(), 1);
    assert_eq!(outcomes.borrow()[0].0, 1);
    assert!(outcomes.borrow()[0].1.is_ok());
    assert_eq!(&*closed.borrow(), &[1]);
}
#[test]
fn retired_closer_fresh_root_refuses_adapterless_dependency_without_old_code_fallback() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("cleanup-retirement", json!(null)),
        driver.clone(),
    );
    let effects = Rc::new(RefCell::new(Vec::new()));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let closed = Rc::new(RefCell::new(Vec::new()));
    driver
        .run_until(
            host.load_executable(
                vec![
                    provider(1, effects.clone()),
                    consumer(1, outcomes.clone(), closed.clone()),
                ],
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    driver
        .run_until(host.reload(
            vec![
                provider(2, effects.clone()),
                consumer(2, outcomes.clone(), closed.clone()),
            ],
            ReloadOptions::default(),
        ))
        .unwrap();
    assert!(
        outcomes.borrow().is_empty(),
        "V2 keeps the previous version until retirement"
    );
    let report = driver
        .run_until(host.reload(vec![], ReloadOptions::default()))
        .unwrap();
    assert_eq!(report.current_version(), 3);
    driver.run_until_stalled();
    let outcomes = outcomes.borrow();
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].0 >= 2,
        "cleanup creates a genuinely fresh current root"
    );
    let error = outcomes[0].1.as_ref().unwrap_err();
    assert!(
        matches!(
            (error.code, error.reason.as_str()),
            (ErrorCode::ContractMismatch, "adapter") | (ErrorCode::Superseded, "code_version")
        ),
        "old cleanup cannot silently invoke obsolete provider code: {error:?}"
    );
    assert!(effects.borrow().is_empty());
    assert!(
        host.errors()
            .iter()
            .any(|error| error.reason == "resource_cleanup")
    );
    assert!(
        !host
            .errors()
            .iter()
            .any(|error| error.reason == "host_failed")
    );
    drop(outcomes);
    let current = driver
        .run_until(host.get(provider_token(2), json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(
        driver
            .run_until(current.call_json("read", None, None))
            .unwrap(),
        Some(json!(2))
    );
    current.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(&*closed.borrow(), &[1, 2]);
}
