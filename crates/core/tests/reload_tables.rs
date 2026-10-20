use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

#[test]
fn equal_metadata_tables_keep_exact_old_and_new_operation_admission() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("table-epochs", json!(null)),
        driver.clone(),
    );
    let registry = Registry::new("epoch-operations", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let contract = Rc::new(
        Contract::local(
            "table-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("table.value@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let definition = |version: u64| {
        let registry = registry.clone();
        let contract = contract.clone();
        ExecutableFacet::foreign("table-owner-facet", Vec::new(), move |_, setup| {
            let registry = registry.clone();
            setup.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    declaration.register_method(
                        &registry,
                        "entry",
                        json!({"same":"metadata"}),
                        "read",
                    )?;
                    Ok(
                        Implementation::new().method("read", move |(): (), context| {
                            async move { Ok((version, context.code_version())) }.boxed_local()
                        }),
                    )
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    };
    driver
        .run_until(async {
            host.install_executable(definition(1), LoadOptions::default())?
                .await?;
            let replacement = definition(2);
            let core = host.clone();
            host.root(move |old_context| {
                async move {
                    let old_table = view.table(Some(&old_context))?;
                    let old_operation =
                        old_table.get("entry").unwrap().operation().unwrap().clone();
                    assert_eq!(
                        old_operation
                            .call::<(), (u64, u64)>(&old_context, ())
                            .await?,
                        (1, 1)
                    );
                    core.reload(vec![replacement], ReloadOptions::default())
                        .await?;
                    let current = view.table(None)?;
                    let current_operation =
                        current.get("entry").unwrap().operation().unwrap().clone();
                    assert_eq!(old_table.code_version(), 1);
                    assert_eq!(current.code_version(), 2);
                    assert_eq!(
                        old_table.get("entry").unwrap().data(),
                        current.get("entry").unwrap().data()
                    );
                    assert!(!old_operation.same_target(&current_operation));
                    let old_for_new = old_operation.clone();
                    core.root(move |new_context| {
                        async move {
                            assert_eq!(
                                current_operation
                                    .call::<(), (u64, u64)>(&new_context, ())
                                    .await?,
                                (2, 2)
                            );
                            assert_eq!(
                                old_for_new
                                    .call::<(), (u64, u64)>(&new_context, ())
                                    .await
                                    .unwrap_err()
                                    .code,
                                ErrorCode::Superseded
                            );
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .await?;
                    assert_eq!(
                        old_operation
                            .call::<(), (u64, u64)>(&old_context, ())
                            .await?,
                        (1, 1)
                    );
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            host.close().await
        })
        .unwrap();
}
#[test]
fn candidate_registry_conflict_preserves_old_table_and_closes_only_new_resource() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("table-conflict", json!(null)),
        driver.clone(),
    );
    let registry = Registry::new("conflicting", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let closed_new = Rc::new(Cell::new(0));
    let closed_old = Rc::new(Cell::new(0));
    let facet = |name: &str, key: &str, version: u64| {
        let token = Rc::new(
            Contract::local(format!("table.{name}"), 1, json!({}), BTreeMap::new()).unwrap(),
        );
        let registry = registry.clone();
        let key = key.to_owned();
        let closed = if version == 1 {
            closed_old.clone()
        } else {
            closed_new.clone()
        };
        ExecutableFacet::foreign(name, Vec::new(), move |_, setup| {
            let registry = registry.clone();
            let key = key.clone();
            let closed = closed.clone();
            setup.serve(
                token.clone(),
                Rc::new(move |_, _, declaration| {
                    declaration.register(&registry, key.clone(), json!({"version":version}))?;
                    let closed = closed.clone();
                    let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                        .with_close(move |_| {
                            closed.set(closed.get() + 1);
                            async { Ok(()) }.boxed_local()
                        });
                    resource.version = version;
                    declaration.resource("value", resource)?;
                    Ok(Implementation::new())
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    };
    driver
        .run_until(async {
            host.load_executable(
                vec![facet("first", "a", 1), facet("second", "b", 1)],
                LoadOptions::default(),
            )?
            .await?;
            let before = view.table(None)?;
            let notifications = Rc::new(Cell::new(0));
            let observed = notifications.clone();
            let subscription = view.subscribe(move |_| {
                observed.set(observed.get() + 1);
                Ok(())
            })?;
            let error = host
                .reload(vec![facet("second", "a", 2)], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.reason, "registry_conflict");
            assert_eq!(closed_new.get(), 1);
            assert_eq!(closed_old.get(), 0);
            assert_eq!(
                notifications.get(),
                1,
                "aborted candidate publishes no registry event"
            );
            let after = view.table(None)?;
            assert_eq!(after.code_version(), 1);
            for key in ["a", "b"] {
                assert_eq!(
                    before.get(key).unwrap().data(),
                    after.get(key).unwrap().data()
                );
            }
            drop(subscription);
            host.close().await?;
            assert_eq!(closed_old.get(), 2);
            Ok::<_, RuntimeError>(())
        })
        .unwrap();
}
