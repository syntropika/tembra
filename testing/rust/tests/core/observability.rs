use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    observability::{Operation, Outcome},
    *,
};

#[test]
fn claims_are_distinct_and_physical_release_follows_logical_result() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("observed", json!(null))
            .with_observability(ObservationOptions::new(1024, 1).unwrap()),
        driver.clone(),
    );
    let observations = host.observations().unwrap();
    let contract = Rc::new(
        Contract::local(
            "observed",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("u32@1".into())),
                    result: Some(ValueSchema::Native("u32@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let physical_id = Rc::new(RefCell::new(None));
    let observed_id = physical_id.clone();
    let lease = Rc::new(RefCell::new(None));
    let saved = lease.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let owner = declaration.invocation_owner();
            let saved = saved.clone();
            let observed_id = observed_id.clone();
            Ok(
                Implementation::new().method("run", move |value: u32, context| {
                    *observed_id.borrow_mut() =
                        Some((context.host_id(), context.observation_id().unwrap()));
                    *saved.borrow_mut() = Some(owner.retain(&context).unwrap());
                    async move { Ok(value + 1) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let first = driver
        .run_until(host.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let second = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(first.object_id(), second.object_id());
    assert_eq!(
        driver
            .run_until(first.call::<u32, u32>("run", 3, None))
            .unwrap(),
        4
    );
    let error = driver
        .run_until(second.call::<u8, u32>("run", 1, None))
        .unwrap_err();
    assert_eq!(error.reason, "native_type");
    let events = observations.drain(1024);
    let claims: Vec<_> = events
        .iter()
        .filter(|event| event.operation == Operation::Claim && event.outcome.is_none())
        .collect();
    assert_eq!(claims.len(), 2);
    assert_ne!(claims[0].id, claims[1].id);
    assert!(
        events
            .iter()
            .any(|event| event.operation == Operation::NativeCall
                && event.outcome == Some(Outcome::Ok)
                && event.root.is_some())
    );
    assert!(
        events
            .iter()
            .any(|event| event.operation == Operation::NativeCall
                && matches!(event.outcome, Some(Outcome::Error { .. })))
    );
    let physical = events
        .iter()
        .find(|event| event.operation == Operation::PhysicalInvocation)
        .unwrap();
    assert!(physical.parent.is_some());
    let id = physical.id;
    assert_eq!(*physical_id.borrow(), Some((physical.host, id)));
    assert!(
        !events
            .iter()
            .any(|event| event.id == id && event.outcome.is_some())
    );
    drop(lease.borrow_mut().take());
    assert!(
        observations
            .drain(1024)
            .iter()
            .any(|event| event.id == id && event.outcome == Some(Outcome::Settled))
    );
    first.release();
    second.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(observations.statistics().dropped_events, 0);
}

#[test]
fn disabled_collection_keeps_the_public_semantics() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("off", json!(null)), driver.clone());
    assert!(host.observations().is_none());
    driver
        .run_until(host.root(|context| {
            async move {
                assert!(context.observe(Operation::Hook, "disabled").is_none());
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn cancelled_scopes_record_signals_without_claiming_physical_termination() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("signals", json!(null))
            .with_observability(ObservationOptions::new(64, 1).unwrap()),
        driver.clone(),
    );
    let observations = host.observations().unwrap();
    driver
        .run_until(host.root(|context| {
            async move {
                let (child, guard) = context.child_call(json!(null))?;
                child.cancel(RuntimeError::aborted("requested", "diagnostic test"));
                assert!(child.cancellation().is_cancelled());
                assert!(child.check(child.host_id()).is_err());
                drop(guard);
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    driver.run_until(host.close()).unwrap();
    let events = observations.drain(64);
    assert!(
        events
            .iter()
            .any(|event| event.operation == Operation::Scope && event.outcome.is_none())
    );
    assert!(
        events
            .iter()
            .any(|event| event.operation == Operation::Cancellation
                && matches!(event.outcome, Some(Outcome::Error { .. })))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.operation == Operation::PhysicalInvocation)
    );
}

#[test]
fn context_acquisition_links_its_actual_claim_without_reassigning_claim_ownership() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("context-get", json!(null))
            .with_observability(ObservationOptions::new(256, 1).unwrap()),
        driver.clone(),
    );
    let contract = Rc::new(Contract::local("empty", 1, json!({}), BTreeMap::new()).unwrap());
    host.serve(
        contract.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default(),
    )
    .unwrap();
    let (root, first, second) = driver
        .run_until(host.root(move |context| {
            async move {
                let first = context
                    .get(contract.clone(), json!({}), GetOptions::default())
                    .await?;
                let second = context
                    .get(contract, json!({}), GetOptions::default())
                    .await?;
                assert_eq!(first.object_id(), second.object_id());
                Ok((
                    context.root_id(),
                    first.observation_claim_id().unwrap(),
                    second.observation_claim_id().unwrap(),
                ))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_ne!(first, second);
    driver.run_until(host.close()).unwrap();
    let events = observations(&host);
    for claim in [first, second] {
        assert!(
            events
                .iter()
                .any(|event| event.operation == Operation::Acquisition
                    && event.root == Some(root)
                    && event.claim == Some(claim)
                    && event.parent.is_some()
                    && event.outcome == Some(Outcome::Ok))
        );
        assert!(
            events
                .iter()
                .any(|event| event.operation == Operation::Claim
                    && event.id == claim
                    && event.root.is_none()
                    && event.outcome.is_none())
        );
    }
}
fn observations(host: &Host) -> Vec<Observation> {
    host.observations().unwrap().drain(4096)
}

#[test]
fn retained_diagnostics_do_not_retain_scheduler_or_its_pending_tasks() {
    struct Marker(Rc<std::cell::Cell<bool>>);
    impl Drop for Marker {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    struct Scheduler {
        inner: Rc<ManualDriver>,
        dropped: Rc<std::cell::Cell<bool>>,
    }
    impl Drop for Scheduler {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }
    impl Driver for Scheduler {
        fn spawn(&self, future: futures::future::LocalBoxFuture<'static, ()>) {
            self.inner.spawn(future);
        }
        fn sleep(
            &self,
            duration: std::time::Duration,
        ) -> futures::future::LocalBoxFuture<'static, ()> {
            self.inner.sleep(duration)
        }
        fn now(&self) -> std::time::Duration {
            self.inner.now()
        }
    }
    let dropped = Rc::new(std::cell::Cell::new(false));
    let task_dropped = Rc::new(std::cell::Cell::new(false));
    let driver = Rc::new(Scheduler {
        inner: ManualDriver::new(),
        dropped: dropped.clone(),
    });
    let host = Host::new(
        HostOptions::new("weak-clock", json!(null))
            .with_observability(ObservationOptions::new(64, 1).unwrap()),
        driver.clone(),
    );
    let observations = host.observations().unwrap();
    let context = driver
        .inner
        .run_until(host.root(|context| async move { Ok(context) }.boxed_local()))
        .unwrap();
    assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
    let mut span = context
        .observe(Operation::Executor, "retained-diagnostic")
        .unwrap();
    let marker = Marker(task_dropped.clone());
    driver.spawn(
        async move {
            let _marker = marker;
            futures::future::pending::<()>().await;
        }
        .boxed_local(),
    );
    driver.inner.run_until_stalled();
    driver.inner.run_until(host.close()).unwrap();
    drop(host);
    drop(driver);
    assert!(dropped.get(), "diagnostic handles retained the Driver");
    assert!(
        task_dropped.get(),
        "diagnostic handles retained pending scheduler work"
    );
    span.finish(Outcome::Ok);
    assert!(observations.statistics().clock_unavailable > 0);
    let events = observations.drain(64);
    assert!(events.iter().any(|event| event.timestamp_ns.is_none()));
    let export = tembra_core::observability::chrome_trace(&events);
    assert!(export["omitted_missing_clock"].as_u64().unwrap() > 0);
    assert!(
        export["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["ts"].is_number())
    );
    assert!(
        tembra_core::observability::summarize(&events)
            .operations
            .values()
            .any(|summary| summary.missing_duration > 0)
    );
    drop(context);
}

#[test]
fn rejected_foreign_contexts_never_enter_the_target_collection_identity_domain() {
    let driver = ManualDriver::new();
    let origin = Host::new(
        HostOptions::new("origin", json!(null))
            .with_observability(ObservationOptions::new(256, 1).unwrap()),
        driver.clone(),
    );
    let target = Host::new(
        HostOptions::new("target", json!(null))
            .with_observability(ObservationOptions::new(256, 1).unwrap()),
        driver.clone(),
    );
    let contract = Rc::new(
        Contract::local(
            "two-routes",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "native".into(),
                    Member::Method {
                        args: Some(ValueSchema::Native("u32@1".into())),
                        result: Some(ValueSchema::Native("u32@1".into())),
                    },
                ),
                (
                    "portable".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    target
        .serve(
            contract.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new()
                    .method("native", |value: u32, _| {
                        async move { Ok(value) }.boxed_local()
                    })
                    .json_method("portable", |value, _| {
                        async move { Ok(value) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let reference = driver
        .run_until(target.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let target_ids: Vec<_> = observations(&target).iter().map(|event| event.id).collect();
    driver
        .run_until(origin.root(move |context| {
            async move {
                assert!(
                    target_ids.contains(&context.observation_id().unwrap()),
                    "fixture must exercise coincident collection IDs"
                );
                assert_eq!(
                    reference
                        .call::<u32, u32>("native", 1, Some(&context))
                        .await
                        .unwrap_err()
                        .reason,
                    "foreign_ctx"
                );
                assert_eq!(
                    reference
                        .call_json("portable", Some(json!(1)), Some(&context))
                        .await
                        .unwrap_err()
                        .reason,
                    "foreign_ctx"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let events = observations(&target);
    let calls: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.operation,
                Operation::NativeCall | Operation::PortableCall
            )
        })
        .collect();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|event| event.parent.is_none()
        && event.root.is_none()
        && event.code_version.is_none()));
    driver.run_until(origin.close()).unwrap();
    driver.run_until(target.close()).unwrap();
}

#[test]
fn registry_reads_reject_foreign_contexts_without_false_correlation() {
    let driver = ManualDriver::new();
    let origin = Host::new(
        HostOptions::new("registry-origin", json!(null))
            .with_observability(ObservationOptions::new(64, 1).unwrap()),
        driver.clone(),
    );
    let target = Host::new(
        HostOptions::new("registry-target", json!(null))
            .with_observability(ObservationOptions::new(64, 1).unwrap()),
        driver.clone(),
    );
    let registry = Registry::new("observed-registry", ConflictPolicy::Last).unwrap();
    let view = target.registry(&registry).unwrap();
    let foreign_view = view.clone();
    driver
        .run_until(origin.root(move |context| {
            async move {
                assert_eq!(
                    foreign_view.table(Some(&context)).err().unwrap().reason,
                    "foreign_ctx"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let refused: Vec<_> = observations(&target)
        .into_iter()
        .filter(|event| event.operation == Operation::RegistryRead)
        .collect();
    assert_eq!(refused.len(), 2);
    assert!(refused.iter().all(|event| {
        event.root.is_none() && event.parent.is_none() && event.code_version.is_none()
    }));
    assert!(matches!(
        &refused[1].outcome,
        Some(Outcome::Error { reason, .. }) if reason == "foreign_ctx"
    ));
    let expected = driver
        .run_until(target.root(move |context| {
            async move {
                assert!(view.table(Some(&context))?.is_empty());
                Ok((
                    context.root_id(),
                    context.observation_id(),
                    context.code_version(),
                ))
            }
            .boxed_local()
        }))
        .unwrap();
    let accepted: Vec<_> = observations(&target)
        .into_iter()
        .filter(|event| event.operation == Operation::RegistryRead)
        .collect();
    assert_eq!(accepted.len(), 2);
    let completed = accepted
        .iter()
        .find(|event| event.outcome == Some(Outcome::Ok))
        .unwrap();
    assert_eq!(completed.root, Some(expected.0));
    assert_eq!(completed.parent, expected.1);
    assert_eq!(completed.code_version, Some(expected.2));
    driver.run_until(origin.close()).unwrap();
    driver.run_until(target.close()).unwrap();
}
