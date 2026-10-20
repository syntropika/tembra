use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

fn token() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "epoch-service",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("epoch.answer@1".into())),
                },
            )]),
        )
        .unwrap(),
    )
}
fn facet(token: Rc<Contract>, value: u64) -> ExecutableFacet {
    ExecutableFacet::foreign("versioned", Vec::new(), move |_, declaration| {
        declaration.serve(
            token.clone(),
            Rc::new(move |_, _, _| {
                Ok(
                    Implementation::new().method("read", move |(): (), context| {
                        async move { Ok((value, context.code_version())) }.boxed_local()
                    }),
                )
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..ServeOptions::default()
            },
        )
    })
    .unwrap()
}
#[test]
fn empty_reload_advances_real_roots_without_replaying_unchanged_factory() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("epochs", json!(null)), driver.clone());
    let token = token();
    driver
        .run_until(async {
            host.install_executable(facet(token.clone(), 11), LoadOptions::default())?
                .await?;
            let reference = host
                .get(token.clone(), json!({}), GetOptions::default())
                .await?;
            assert_eq!(
                reference.call::<(), (u64, u64)>("read", (), None).await?,
                (11, 1)
            );
            let report = host.reload(Vec::new(), ReloadOptions::default()).await?;
            assert_eq!(report.current_version(), 2);
            assert!(report.prepared_objects().is_empty());
            assert_eq!(
                reference.call::<(), (u64, u64)>("read", (), None).await?,
                (11, 2)
            );
            reference.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn compatible_replacement_selects_new_implementation_under_actual_epoch() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("epochs", json!(null)), driver.clone());
    let token = token();
    driver
        .run_until(async {
            host.install_executable(facet(token.clone(), 11), LoadOptions::default())?
                .await?;
            let reference = host
                .get(token.clone(), json!({}), GetOptions::default())
                .await?;
            let report = host
                .reload(vec![facet(token.clone(), 22)], ReloadOptions::default())
                .await?;
            assert_eq!(report.current_version(), 2);
            assert_eq!(report.prepared_objects().len(), 1);
            assert_eq!(
                reference.call::<(), (u64, u64)>("read", (), None).await?,
                (22, 2)
            );
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn previous_root_remains_callable_after_commit_and_retires_before_third_version() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("epochs", json!(null)), driver.clone());
    let token = token();
    driver
        .run_until(async {
            host.install_executable(facet(token.clone(), 11), LoadOptions::default())?
                .await?;
            let reference = host
                .get(token.clone(), json!({}), GetOptions::default())
                .await?;
            let old_reference = reference.clone();
            let (send_context, receive_context) = futures::channel::oneshot::channel();
            let (end, ending) = futures::channel::oneshot::channel();
            let owner = host.clone();
            driver.spawn(
                async move {
                    owner
                        .root(move |context| {
                            async move {
                                assert_eq!(
                                    old_reference
                                        .call::<(), (u64, u64)>("read", (), Some(&context))
                                        .await?,
                                    (11, 1)
                                );
                                assert!(send_context.send(context).is_ok());
                                let _ = ending.await;
                                Ok(())
                            }
                            .boxed_local()
                        })
                        .await
                        .unwrap();
                }
                .boxed_local(),
            );
            let previous = receive_context.await.unwrap();
            let report = host
                .reload(vec![facet(token.clone(), 22)], ReloadOptions::default())
                .await?;
            assert_eq!(report.current_version(), 2);
            assert_eq!(report.previous_version(), Some(1));
            assert!(
                !previous.cancellation().is_cancelled(),
                "V2 commit must not force-retire V1"
            );
            assert_eq!(
                reference
                    .call::<(), (u64, u64)>("read", (), Some(&previous))
                    .await?,
                (11, 1)
            );
            assert_eq!(
                reference.call::<(), (u64, u64)>("read", (), None).await?,
                (22, 2)
            );
            let report = host
                .reload(
                    vec![facet(token.clone(), 33)],
                    ReloadOptions {
                        retire_deadline: std::time::Duration::from_millis(1),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(report.current_version(), 3);
            assert_eq!(report.previous_version(), Some(2));
            let refused = reference
                .call::<(), (u64, u64)>("read", (), Some(&previous))
                .await
                .unwrap_err();
            assert_eq!(refused.code, ErrorCode::Superseded);
            assert_eq!(
                reference.call::<(), (u64, u64)>("read", (), None).await?,
                (33, 3)
            );
            end.send(()).unwrap();
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn invalid_reload_deadlines_refuse_before_setup_or_epoch_and_slot_changes() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("options", json!(null)), driver.clone());
    driver.run_until(driver.sleep(std::time::Duration::from_nanos(1)));
    let setups = Rc::new(std::cell::Cell::new(0));
    let observed = setups.clone();
    let candidate = ExecutableFacet::foreign("invalid-options", Vec::new(), move |_, _| {
        observed.set(observed.get() + 1);
        Ok(())
    })
    .unwrap();
    let zero = std::time::Duration::ZERO;
    let max = std::time::Duration::MAX;
    let options = [
        ReloadOptions {
            gate_deadline: zero,
            ..Default::default()
        },
        ReloadOptions {
            retire_deadline: zero,
            ..Default::default()
        },
        ReloadOptions {
            task_stop_deadline: zero,
            ..Default::default()
        },
        ReloadOptions {
            gate_deadline: max,
            ..Default::default()
        },
        ReloadOptions {
            retire_deadline: max,
            ..Default::default()
        },
        ReloadOptions {
            task_stop_deadline: max,
            ..Default::default()
        },
    ];
    for options in options {
        let error = driver
            .run_until(host.reload(vec![candidate.clone()], options))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Invalid);
        assert_eq!(error.reason, "reload_options");
    }
    assert_eq!(setups.get(), 0);
    assert_eq!(
        driver
            .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
            .unwrap(),
        1
    );
    assert_eq!(
        driver
            .run_until(host.reload(Vec::new(), ReloadOptions::default()))
            .unwrap()
            .current_version(),
        2
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn healthy_unchanged_task_alias_is_not_reported_as_lingering_retired_work() {
    use std::cell::Cell;
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("healthy-task", json!(null)),
        driver.clone(),
    );
    let contract = Rc::new(Contract::local("task-alias", 1, json!({}), BTreeMap::new()).unwrap());
    let starts = Rc::new(Cell::new(0));
    let running = starts.clone();
    host.serve(
        contract,
        Rc::new(move |_, _, declaration| {
            let running = running.clone();
            declaration.task(move |scope| {
                running.set(running.get() + 1);
                async move {
                    scope.signal().cancelled().await;
                    Ok(())
                }
                .boxed_local()
            })?;
            Ok(Implementation::new())
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    driver.run_until_stalled();
    assert_eq!(starts.get(), 1);
    let first = driver
        .run_until(host.reload(vec![], ReloadOptions::default()))
        .unwrap();
    assert_eq!(first.current_version(), 2);
    let second = driver
        .run_until(host.reload(vec![], ReloadOptions::default()))
        .unwrap();
    assert_eq!(second.current_version(), 3);
    assert_eq!(
        starts.get(),
        1,
        "unchanged declaration retains its original task"
    );
    assert_eq!(
        second.lingering_physical(),
        0,
        "an active unchanged task is not stopped old-generation work"
    );
    driver.run_until(host.close()).unwrap();
}
