use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell as Counter, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

fn provider_token(version: u64, extra: bool) -> Rc<Contract> {
    let mut members = BTreeMap::from([(
        "value".into(),
        Member::State {
            schema: json!({"type":"integer"}),
        },
    )]);
    if extra {
        members.insert(
            "extra".into(),
            Member::State {
                schema: json!({"type":"integer"}),
            },
        );
    }
    Rc::new(Contract::local("adapter-provider", version, json!({}), members).unwrap())
}
fn consumer_token() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "adapter-consumer",
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
fn provider(
    contract_version: u64,
    state_version: u64,
    extra: bool,
    adapters: Rc<Counter<usize>>,
    closes: Rc<Counter<usize>>,
) -> ExecutableFacet {
    let mut options = ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    };
    if contract_version == 2 {
        options = options.with_adapter(
            1,
            Rc::new(move |implementation| {
                adapters.set(adapters.get() + 1);
                Ok(implementation.as_ref().clone())
            }),
        );
    }
    ExecutableFacet::foreign("adapter-provider", vec![], move |_, setup| {
        let closes = closes.clone();
        setup.serve(
            provider_token(contract_version, extra),
            Rc::new(move |_, _, declaration| {
                let closes = closes.clone();
                declaration.resource(
                    "owned",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            closes.set(closes.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let value = declaration.state(
                    "value",
                    StateDeclaration::new(json!({"type":"integer"}), json!(7))
                        .version(state_version)
                        .with_migrate(|_, _| Ok(json!(9))),
                )?;
                let implementation = Implementation::new().state("value", value.readable());
                if extra {
                    let extra = declaration.state(
                        "extra",
                        StateDeclaration::new(json!({"type":"integer"}), json!(11)),
                    )?;
                    Ok(implementation.state("extra", extra.readable()))
                } else {
                    Ok(implementation)
                }
            }),
            options.clone(),
        )
    })
    .unwrap()
}
fn consumer(import: bool, release: bool) -> ExecutableFacet {
    ExecutableFacet::foreign("adapter-consumer", vec![], move |_, setup| {
        setup.serve(
            consumer_token(),
            Rc::new(move |_, _, declaration| {
                let value = if import {
                    let dependency = declaration.get(
                        provider_token(1, false),
                        json!({}),
                        GetOptions::default(),
                    )?;
                    let source = dependency.readable("value")?;
                    if release {
                        dependency.release();
                    }
                    declaration.derive(vec![source], json!({"type":"integer"}), |values| {
                        Ok(values[0].value().clone())
                    })?
                } else {
                    declaration
                        .state(
                            "value",
                            StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                        )?
                        .readable()
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
fn extra_state_identity_adapter(import: bool) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("extra-state-adapter", json!(null)),
        driver.clone(),
    );
    let adapters = Rc::new(Counter::new(0));
    let closes = Rc::new(Counter::new(0));
    driver
        .run_until(async {
            host.install_executable(
                provider(1, 1, false, adapters.clone(), closes.clone()),
                LoadOptions::default(),
            )?
            .await?;
            let retained = host
                .get(provider_token(1, false), json!({}), GetOptions::default())
                .await?;
            let old_id = retained.object_id();
            let value = retained.readable("value")?;
            let consumer = if import {
                host.install_executable(consumer(false, false), LoadOptions::default())?
                    .await?;
                Some(
                    host.get(consumer_token(), json!({}), GetOptions::default())
                        .await?,
                )
            } else {
                None
            };
            let mut replacements = vec![provider(2, 2, true, adapters.clone(), closes.clone())];
            if import {
                replacements.push(self::consumer(true, false));
            }
            let report = host.reload(replacements, ReloadOptions::default()).await?;
            assert_eq!(report.current_version(), 2);
            assert_eq!(value.snapshot()?.value(), &json!(9));
            assert_eq!(retained.object_id(), old_id);
            let Err(hidden) = retained.readable("extra") else {
                panic!("the old consumer must not expose the extra provider state");
            };
            assert_eq!(hidden.code, ErrorCode::NotInVariant);
            if let Some(consumer) = &consumer {
                assert_eq!(consumer.readable("value")?.snapshot()?.value(), &json!(9));
            }
            let current = host
                .get(provider_token(2, true), json!({}), GetOptions::default())
                .await?;
            assert_eq!(current.readable("extra")?.snapshot()?.value(), &json!(11));
            assert_eq!(
                adapters.get(),
                1,
                "one cached identity adapter serves both preparation paths"
            );
            assert_eq!(closes.get(), 0);
            host.reload(vec![], ReloadOptions::default()).await?;
            assert_eq!(adapters.get(), 1);
            if let Some(consumer) = consumer {
                consumer.release();
            }
            retained.release();
            current.release();
            host.close().await?;
            assert_eq!(closes.get(), 1);
            Ok::<_, RuntimeError>(())
        })
        .unwrap();
}
#[test]
fn identity_adapter_keeps_extra_provider_state_out_of_a_candidate_dependency_view() {
    extra_state_identity_adapter(true);
}
#[test]
fn identity_adapter_keeps_extra_provider_state_out_of_an_external_consumer_view() {
    extra_state_identity_adapter(false);
}
#[test]
fn released_candidate_dependency_cannot_hydrate_through_a_private_source_alias() {
    let driver = ManualDriver::new();
    let mut options = HostOptions::new("released-state-import", json!(null));
    options.ready_timeout = Duration::from_millis(3);
    let host = Host::new(options, driver.clone());
    let adapters = Rc::new(Counter::new(0));
    let closes = Rc::new(Counter::new(0));
    driver
        .run_until(async {
            host.install_executable(
                provider(1, 1, false, adapters.clone(), closes.clone()),
                LoadOptions::default(),
            )?
            .await?;
            host.install_executable(consumer(false, false), LoadOptions::default())?
                .await?;
            let selected = host
                .get(consumer_token(), json!({}), GetOptions::default())
                .await?;
            let value = selected.readable("value")?;
            let now = driver.now();
            let error = host
                .reload(
                    vec![
                        provider(1, 2, false, adapters, closes.clone()),
                        consumer(true, true),
                    ],
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Released);
            assert_eq!(
                driver.now(),
                now,
                "ended input refuses without waiting a readiness deadline"
            );
            assert_eq!(value.snapshot()?.value(), &json!(0));
            assert_eq!(
                host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
                    .await?,
                1
            );
            assert_eq!(closes.get(), 0);
            assert_eq!(
                host.reload(vec![], ReloadOptions::default())
                    .await?
                    .current_version(),
                2
            );
            assert_eq!(value.snapshot()?.value(), &json!(0));
            selected.release();
            host.close().await?;
            assert_eq!(closes.get(), 1);
            Ok::<_, RuntimeError>(())
        })
        .unwrap();
}
