use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell as Counter, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

#[derive(Clone, Copy)]
enum Use {
    Resource,
    State,
    InitializerCall,
}
fn provider_token(version: u64) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "dependency-source",
            version,
            json!({}),
            BTreeMap::from([
                (
                    "value".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
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
    Rc::new(
        Contract::local(
            "dependency-consumer",
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
fn provider(version: u64, entered: Rc<Counter<usize>>) -> ExecutableFacet {
    provider_options(
        version,
        entered,
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
}
fn provider_options(
    version: u64,
    entered: Rc<Counter<usize>>,
    options: ServeOptions,
) -> ExecutableFacet {
    ExecutableFacet::foreign("dependency-source", vec![], move |_, setup| {
        let entered = entered.clone();
        setup.serve(
            provider_token(version),
            Rc::new(move |_, _, declaration| {
                entered.set(entered.get() + 1);
                let value = declaration.state(
                    "value",
                    StateDeclaration::new(json!({"type":"integer"}), json!(7))
                        .version(version)
                        .with_migrate(|_, _| Ok(json!(9))),
                )?;
                Ok(Implementation::new()
                    .state("value", value.readable())
                    .json_method("read", |_, _| async { Ok(Some(json!(7))) }.boxed_local()))
            }),
            options.clone(),
        )
    })
    .unwrap()
}
fn consumer(dependency: Option<(Rc<Contract>, Use)>, opens: Rc<Counter<usize>>) -> ExecutableFacet {
    ExecutableFacet::foreign("dependency-consumer", vec![], move |_, setup| {
        let dependency = dependency.clone();
        let opens = opens.clone();
        setup.serve(
            consumer_token(),
            Rc::new(move |_, _, declaration| {
                let Some((token, usage)) = &dependency else {
                    let value = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                    )?;
                    return Ok(Implementation::new().state("value", value.readable()));
                };
                let reference = declaration.get(token.clone(), json!({}), GetOptions::default())?;
                let value = match usage {
                    Use::Resource => {
                        let opens = opens.clone();
                        let mut resource = ResourceDeclaration::new(move |_| {
                            opens.set(opens.get() + 1);
                            async { Ok(()) }.boxed_local()
                        });
                        resource.deps.push(ResourceDependency::Reference(reference));
                        declaration.resource("dependent", resource)?;
                        declaration
                            .state(
                                "prepared-value",
                                StateDeclaration::new(json!({"type":"integer"}), json!(7)),
                            )?
                            .readable()
                    }
                    Use::State => declaration.derive(
                        vec![reference.readable("value")?],
                        json!({"type":"integer"}),
                        |values| Ok(values[0].value().clone()),
                    )?,
                    Use::InitializerCall => declaration
                        .state(
                            "prepared-value",
                            StateDeclaration::with_initializer(
                                json!({"type":"integer"}),
                                move |context| {
                                    let reference = reference.clone();
                                    async move {
                                        reference
                                            .call_json("read", None, Some(&context))
                                            .await?
                                            .ok_or_else(|| {
                                                RuntimeError::invalid("result", "result absent")
                                            })
                                    }
                                    .boxed_local()
                                },
                            ),
                        )?
                        .readable(),
                };
                Ok(Implementation::new().state("value", value))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap()
}
fn run(usage: Use, bump: bool, adapted: bool) {
    let driver = ManualDriver::new();
    let mut options = HostOptions::new("candidate-dependencies", json!(null));
    options.ready_timeout = Duration::from_millis(3);
    let host = Host::new(options, driver.clone());
    let provider_factories = Rc::new(Counter::new(0));
    let opens = Rc::new(Counter::new(0));
    let adapters = Rc::new(Counter::new(0));
    driver
        .run_until(async {
            host.install_executable(
                provider(1, provider_factories.clone()),
                LoadOptions::default(),
            )?
            .await?;
            host.install_executable(consumer(None, opens.clone()), LoadOptions::default())?
                .await?;
            let selected = host
                .get(consumer_token(), json!({}), GetOptions::default())
                .await?;
            let value = selected.readable("value")?;
            assert_eq!(value.snapshot()?.value(), &json!(0));
            let mut batch = vec![consumer(
                Some((provider_token(if bump && !adapted { 2 } else { 1 }), usage)),
                opens.clone(),
            )];
            if bump {
                let counted = adapters.clone();
                let options = ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                };
                let options = if adapted {
                    options.with_adapter(
                        1,
                        Rc::new(move |implementation| {
                            counted.set(counted.get() + 1);
                            Ok(implementation.as_ref().clone())
                        }),
                    )
                } else {
                    options
                };
                batch.push(provider_options(2, provider_factories.clone(), options));
            }
            let report = host.reload(batch, ReloadOptions::default()).await?;
            assert_eq!(report.current_version(), 2);
            assert_eq!(value.snapshot()?.value(), &json!(if bump { 9 } else { 7 }));
            assert_eq!(provider_factories.get(), if bump { 2 } else { 1 });
            assert_eq!(
                adapters.get(),
                usize::from(adapted),
                "checked candidate view is reused at commit"
            );
            if matches!(usage, Use::Resource) {
                assert_eq!(opens.get(), 1);
            }
            let alias = host.reload(vec![], ReloadOptions::default()).await?;
            assert_eq!(alias.current_version(), 3);
            assert_eq!(value.snapshot()?.value(), &json!(if bump { 9 } else { 7 }));
            assert_eq!(adapters.get(), usize::from(adapted));
            selected.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn new_dependency_on_unchanged_provider_can_open_a_resource() {
    run(Use::Resource, false, false);
}
#[test]
fn new_dependency_on_unchanged_provider_can_import_initialized_state() {
    run(Use::State, false, false);
}
#[test]
fn new_dependency_on_unchanged_provider_can_call_from_initializer() {
    run(Use::InitializerCall, false, false);
}
#[test]
fn pending_provider_state_is_projected_before_joint_commit() {
    run(Use::State, true, false);
}
#[test]
fn pending_adapted_state_uses_one_checked_view_before_joint_commit() {
    run(Use::State, true, true);
}

#[test]
fn changed_pending_provider_call_refuses_before_deadline_or_method_effects() {
    let driver = ManualDriver::new();
    let mut options = HostOptions::new("private-prepare-call", json!(null));
    options.ready_timeout = Duration::from_millis(3);
    let host = Host::new(options, driver.clone());
    let calls = Rc::new(Counter::new(0));
    let effects = calls.clone();
    let candidate = ExecutableFacet::foreign("dependency-source", vec![], move |_, setup| {
        let calls = effects.clone();
        setup.serve(
            provider_token(2),
            Rc::new(move |_, _, declaration| {
                let value = declaration.state(
                    "value",
                    StateDeclaration::new(json!({"type":"integer"}), json!(7)),
                )?;
                let calls = calls.clone();
                Ok(Implementation::new()
                    .state("value", value.readable())
                    .json_method("read", move |_, _| {
                        calls.set(calls.get() + 1);
                        async { Ok(Some(json!(9))) }.boxed_local()
                    }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    driver
        .run_until(async {
            host.install_executable(
                provider(1, Rc::new(Counter::new(0))),
                LoadOptions::default(),
            )?
            .await?;
            host.install_executable(
                consumer(None, Rc::new(Counter::new(0))),
                LoadOptions::default(),
            )?
            .await?;
            let selected = host
                .get(consumer_token(), json!({}), GetOptions::default())
                .await?;
            let now = driver.now();
            let error = host
                .reload(
                    vec![
                        candidate,
                        consumer(
                            Some((provider_token(2), Use::InitializerCall)),
                            Rc::new(Counter::new(0)),
                        ),
                    ],
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::NotReady);
            assert_eq!(error.reason, "candidate_private");
            assert_eq!(driver.now(), now, "refusal is prompt, not a ready timeout");
            assert_eq!(calls.get(), 0);
            assert_eq!(selected.readable("value")?.snapshot()?.value(), &json!(0));
            assert_eq!(
                host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
                    .await?,
                1
            );
            let unchanged = host
                .get(provider_token(1), json!({}), GetOptions::default())
                .await?;
            assert_eq!(
                unchanged.call_json("read", None, None).await?,
                Some(json!(7))
            );
            unchanged.release();
            assert_eq!(
                host.reload(vec![], ReloadOptions::default())
                    .await?
                    .current_version(),
                2
            );
            assert_eq!(selected.readable("value")?.snapshot()?.value(), &json!(0));
            selected.release();
            host.close().await
        })
        .unwrap();
}
