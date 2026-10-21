use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

type Observation = Vec<Option<String>>;

fn token(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "inspect".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("caller.observation@1".into())),
                },
            )]),
        )
        .unwrap(),
    )
}

fn eager() -> ServeOptions {
    ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
}

fn sink(contract: Rc<Contract>) -> NativeFacet {
    NativeFacet::new("caller.sink", vec![], move |_, setup| {
        setup.serve(
            contract.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new().method("inspect", |(): (), context| {
                    async move {
                        Ok(vec![
                            context.caller_facet().map(str::to_owned),
                            context.principal().as_str().map(str::to_owned),
                        ])
                    }
                    .boxed_local()
                }))
            }),
            eager(),
        )
    })
    .unwrap()
}

fn scope(name: &str, saved: Rc<RefCell<Option<FacetScope>>>) -> NativeFacet {
    NativeFacet::new(name, vec![], move |scope, _| {
        *saved.borrow_mut() = Some(scope.clone());
        Ok(())
    })
    .unwrap()
}

async fn inspect(scope: FacetScope, reference: Reference) -> Result<Observation> {
    scope
        .root(move |context| {
            async move { reference.call("inspect", (), Some(&context)).await }.boxed_local()
        })
        .await
}

#[test]
fn facet_roots_attribute_distinct_callers_and_principal_overrides_do_not_impersonate_them() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("caller-host", json!("host-principal")),
        driver.clone(),
    );
    let contract = token("caller.sink");
    let a = Rc::new(RefCell::new(None));
    let b = Rc::new(RefCell::new(None));
    driver
        .run_until(async {
            host.load(
                vec![
                    sink(contract.clone()),
                    scope("caller.a", a.clone()),
                    scope("caller.b", b.clone()),
                ],
                LoadOptions::default(),
            )?
            .await?;
            let reference = host.get(contract, json!({}), GetOptions::default()).await?;
            let caller_scopes = [
                (a.borrow().clone().unwrap(), "caller.a"),
                (b.borrow().clone().unwrap(), "caller.b"),
            ];
            for (scope, name) in caller_scopes {
                assert_eq!(
                    inspect(scope, reference.clone()).await?,
                    vec![Some(name.into()), Some("host-principal".into())]
                );
            }
            assert_eq!(
                reference
                    .call::<(), Observation>("inspect", (), None)
                    .await?[0],
                None
            );
            let held = reference.clone();
            let forged = host
                .root_as(json!("caller.a"), move |context| {
                    async move {
                        held.call::<(), Observation>("inspect", (), Some(&context))
                            .await
                    }
                    .boxed_local()
                })
                .await?;
            assert_eq!(forged[0], None);
            host.close().await
        })
        .unwrap();
    let closed = a.borrow().clone().unwrap();
    assert_eq!(
        driver
            .run_until(closed.root(|_| async { Ok(()) }.boxed_local()))
            .unwrap_err()
            .reason,
        "closed"
    );
}

#[test]
fn nested_calls_attribute_the_immediate_receiver_and_child_scopes_preserve_owned_origin() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("nested-caller", json!(null)),
        driver.clone(),
    );
    let target = token("caller.sink");
    let relay = token("caller.relay");
    let saved = Rc::new(RefCell::new(None));
    let dependency = target.clone();
    let served = relay.clone();
    let relay_facet = NativeFacet::new("caller.relay", vec![], move |_, setup| {
        let dependency = dependency.clone();
        setup.serve(
            served.clone(),
            Rc::new(move |_, _, declaration| {
                let reference =
                    declaration.get(dependency.clone(), json!({}), GetOptions::default())?;
                Ok(
                    Implementation::new().method("inspect", move |(): (), context| {
                        let reference = reference.clone();
                        async move {
                            let caller = context.caller_facet().map(str::to_owned);
                            let (child, _guard) =
                                context.child_call(json!("arbitrary-principal"))?;
                            let downstream: Observation =
                                reference.call("inspect", (), Some(&child)).await?;
                            Ok(vec![caller, downstream[0].clone()])
                        }
                        .boxed_local()
                    }),
                )
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            host.load(
                vec![sink(target), relay_facet, scope("caller.a", saved.clone())],
                LoadOptions::default(),
            )?
            .await?;
            let reference = host.get(relay, json!({}), GetOptions::default()).await?;
            let caller_scope = saved.borrow().clone().unwrap();
            assert_eq!(
                inspect(caller_scope, reference.clone()).await?,
                vec![Some("caller.a".into()), Some("caller.relay".into())]
            );
            assert_eq!(
                reference
                    .call::<(), Observation>("inspect", (), None)
                    .await?,
                vec![None, Some("caller.relay".into())]
            );
            host.close().await
        })
        .unwrap();
}

#[test]
fn managed_task_roots_attribute_owner_and_expired_contexts_cannot_make_owned_calls() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("task-caller", json!(null)), driver.clone());
    let contract = token("caller.sink");
    let (sender, received) = oneshot::channel();
    let sender = Rc::new(RefCell::new(Some(sender)));
    let retained = Rc::new(RefCell::new(None));
    let saved = retained.clone();
    let dependency = contract.clone();
    let task = NativeFacet::new("caller.task", vec![], move |_, setup| {
        let dependency = dependency.clone();
        let sender = sender.clone();
        let saved = saved.clone();
        setup.serve(
            Rc::new(Contract::local("caller.task", 1, json!({}), BTreeMap::new()).unwrap()),
            Rc::new(move |_, _, declaration| {
                let reference =
                    declaration.get(dependency.clone(), json!({}), GetOptions::default())?;
                let sender = sender.clone();
                let saved = saved.clone();
                declaration.task(move |scope| {
                    let reference = reference.clone();
                    let sender = sender.borrow_mut().take().unwrap();
                    let saved = saved.clone();
                    async move {
                        let observation = scope
                            .root(move |context| {
                                *saved.borrow_mut() = Some(context.clone());
                                async move {
                                    reference
                                        .call::<(), Observation>("inspect", (), Some(&context))
                                        .await
                                }
                                .boxed_local()
                            })
                            .await?;
                        sender.send(observation).unwrap();
                        Ok(())
                    }
                    .boxed_local()
                })?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            host.load(vec![sink(contract.clone()), task], LoadOptions::default())?
                .await?;
            assert_eq!(received.await.unwrap()[0], Some("caller.task".into()));
            let reference = host.get(contract, json!({}), GetOptions::default()).await?;
            let expired = retained.borrow().clone().unwrap();
            assert_eq!(
                reference
                    .call::<(), Observation>("inspect", (), Some(&expired))
                    .await
                    .unwrap_err()
                    .reason,
                "expired"
            );
            host.close().await
        })
        .unwrap();
}

#[test]
fn compatible_replacement_and_fresh_hosts_keep_the_installed_facet_name() {
    for _ in 0..2 {
        let driver = ManualDriver::new();
        let host = Host::new(
            HostOptions::new("replace-caller", json!(null)),
            driver.clone(),
        );
        let contract = token("caller.sink");
        let saved = Rc::new(RefCell::new(Vec::new()));
        let scopes = saved.clone();
        let definition = ExecutableFacet::foreign("caller.replaceable", vec![], move |scope, _| {
            scopes.borrow_mut().push(scope.clone());
            Ok(())
        })
        .unwrap();
        driver
            .run_until(async {
                host.install(sink(contract.clone()), LoadOptions::default())?
                    .await?;
                host.install_executable(definition.clone(), LoadOptions::default())?
                    .await?;
                let reference = host.get(contract, json!({}), GetOptions::default()).await?;
                let first = saved.borrow()[0].clone();
                assert_eq!(
                    inspect(first.clone(), reference.clone()).await?[0],
                    Some("caller.replaceable".into())
                );
                host.reload(vec![definition], ReloadOptions::default())
                    .await?;
                let next = saved.borrow()[1].clone();
                assert!(first.signal().is_cancelled());
                assert_eq!(
                    inspect(next, reference.clone()).await?[0],
                    Some("caller.replaceable".into())
                );
                // Retired Facet scopes deliberately retain cleanup-root authority until Host closure.
                assert_eq!(
                    inspect(first, reference).await?[0],
                    Some("caller.replaceable".into())
                );
                host.close().await
            })
            .unwrap();
    }
}
