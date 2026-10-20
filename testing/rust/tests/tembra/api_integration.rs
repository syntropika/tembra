use std::{cell::RefCell, rc::Rc};

use futures::{FutureExt, channel::oneshot};
use tembra::prelude::*;
use tembra::{ErrorCode, ManualDriver, Resource, ResourceDeclaration, core};

#[tembra::contract(local, name = "dx.facet-dependency", version = 1)]
trait Dependency {
    async fn read(&self) -> Result<u64>;
}

struct DependencyService {
    value: Resource<u64>,
}

impl DependencyProvider for DependencyService {
    async fn read(&self, _context: Context) -> Result<u64> {
        Ok(*self.value.value()?)
    }
}

#[tembra::contract(local, name = "dx.facet-consumer", version = 1)]
trait Consumer {
    #[state]
    fn seed(&self) -> u64;
    async fn read(&self) -> Result<u64>;
}

struct ConsumerService {
    dependency: DependencyRef,
    value: Resource<u64>,
    seed: Cell<u64>,
}

impl ConsumerProvider for ConsumerService {
    fn seed(&self) -> Readable<u64> {
        self.seed.readable()
    }

    async fn read(&self, context: Context) -> Result<u64> {
        let dependency = self.dependency.with_context(&context).read().await?;
        Ok(dependency + *self.value.value()?)
    }
}

#[test]
fn typed_facet_records_sync_dependencies_and_awaits_resource_and_state_readiness() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("dx-facets", (), driver.clone())?;
    let events = Rc::new(RefCell::new(Vec::new()));
    let setup_events = events.clone();
    let (gate, wait) = oneshot::channel();
    let wait = Rc::new(RefCell::new(Some(wait)));
    let facet = NativeFacet::new("typed-services", vec![], move |_, setup| {
        setup_events.borrow_mut().push("setup");
        let factory_events = setup_events.clone();
        // Record the eager consumer first; dependencies activate after setup finishes.
        setup.serve_with_options(
            Consumer,
            move |_, _, declaration| {
                factory_events.borrow_mut().push("consumer factory");
                let dependency = declaration.get(Dependency, DependencyInput {})?;
                let repeated = declaration.get(Dependency, DependencyInput {})?;
                assert_eq!(dependency.core().object_id(), repeated.core().object_id());
                assert_eq!(dependency.core().status(), core::RefStatus::Loading);
                let source = dependency.clone();
                let open_events = factory_events.clone();
                let close_events = factory_events.clone();
                let mut resource = ResourceDeclaration::new(move |_| {
                    let source = source.clone();
                    let events = open_events.clone();
                    async move {
                        let value = source.read().await?;
                        events.borrow_mut().push("consumer open");
                        Ok(value + 1)
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let events = close_events.clone();
                    async move {
                        events.borrow_mut().push("consumer close");
                        Ok(())
                    }
                    .boxed_local()
                });
                resource.deps.push(dependency.core().dependency());
                let value = declaration.core_mut().resource("consumer", resource)?;
                assert_eq!(value.value().unwrap_err().code, ErrorCode::NotReady);
                let initialized_value = value.clone();
                let initialize_events = factory_events.clone();
                let seed = declaration.state_with(
                    "seed",
                    StateDeclaration::with_initializer(move |_| {
                        let value = initialized_value.clone();
                        let events = initialize_events.clone();
                        async move {
                            events.borrow_mut().push("consumer initialize");
                            Ok(*value.value()?)
                        }
                    })?,
                )?;
                Ok(ConsumerService {
                    dependency,
                    value,
                    seed,
                })
            },
            core::ServeOptions {
                lifetime: core::Lifetime::Host,
                ..Default::default()
            },
        )?;
        let factory_events = setup_events.clone();
        let wait = wait.clone();
        setup.serve(Dependency, move |_, _, declaration| {
            factory_events.borrow_mut().push("dependency factory");
            let open_events = factory_events.clone();
            let close_events = factory_events.clone();
            let wait = wait.clone();
            let value = declaration.core_mut().resource(
                "dependency",
                ResourceDeclaration::new(move |_| {
                    let wait = wait.borrow_mut().take().unwrap();
                    let events = open_events.clone();
                    async move {
                        events.borrow_mut().push("dependency opening");
                        wait.await.map_err(|_| {
                            RuntimeError::unavailable("gate", "test gate disconnected")
                        })?;
                        events.borrow_mut().push("dependency open");
                        Ok(7_u64)
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let events = close_events.clone();
                    async move {
                        events.borrow_mut().push("dependency close");
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            assert_eq!(value.value().unwrap_err().code, ErrorCode::NotReady);
            Ok(DependencyService { value })
        })?;
        setup_events.borrow_mut().push("setup complete");
        Ok(())
    })?;
    let startup = host.install(facet, core::LoadOptions::default())?;
    assert_eq!(*events.borrow(), ["setup", "setup complete"]);
    driver.run_until_stalled();
    assert_eq!(
        *events.borrow(),
        [
            "setup",
            "setup complete",
            "consumer factory",
            "dependency factory",
            "dependency opening",
        ]
    );
    driver.run_until(async {
        futures::pin_mut!(startup);
        assert!(futures::poll!(startup.as_mut()).is_pending());
        gate.send(()).unwrap();
        startup.await?;
        let consumer = host.get(Consumer, ConsumerInput {}).await?;
        assert_eq!(consumer.seed().value()?, 8);
        assert_eq!(consumer.read().await?, 15);
        assert_eq!(
            &events.borrow()[5..],
            ["dependency open", "consumer open", "consumer initialize"]
        );
        host.close().await?;
        assert_eq!(
            &events.borrow()[8..],
            ["consumer close", "dependency close"]
        );
        assert!(host.core().errors().is_empty());
        Ok(())
    })
}

#[cfg(feature = "native")]
#[test]
fn native_runner_keeps_non_send_work_on_one_thread_and_closes_before_return() -> Result<()> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let setup_events = events.clone();
    let origin = std::thread::current().id();
    let result = tembra::native::run("dx-native-run", (), move |host| async move {
        let events = setup_events.clone();
        host.serve(Dependency, move |_, _, declaration| {
            assert_eq!(std::thread::current().id(), origin);
            let open_events = events.clone();
            let close_events = events.clone();
            let resource = ResourceDeclaration::new(move |_| {
                let events = open_events.clone();
                async move {
                    assert_eq!(std::thread::current().id(), origin);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    events.borrow_mut().push("open");
                    Ok(9_u64)
                }
                .boxed_local()
            })
            .with_close(move |_| {
                let events = close_events.clone();
                async move {
                    assert_eq!(std::thread::current().id(), origin);
                    events.borrow_mut().push("close");
                    Ok(())
                }
                .boxed_local()
            });
            Ok(DependencyService {
                value: declaration.core_mut().resource("value", resource)?,
            })
        })?;
        let dependency = host.get(Dependency, DependencyInput {}).await?;
        let result = dependency.read().await?;
        assert_eq!(std::thread::current().id(), origin);
        setup_events.borrow_mut().push("callback complete");
        Ok(result)
    })?;
    assert_eq!(result, 9);
    assert_eq!(*events.borrow(), ["open", "callback complete", "close"]);
    Ok(())
}

#[cfg(feature = "native")]
#[test]
fn native_runner_preserves_callback_error_after_attempting_failing_cleanup() -> Result<()> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let setup_events = events.clone();
    let callback_error = RuntimeError::invalid("callback", "application refused work")
        .with_details(tembra::serde_json::json!({"phase": "work"}));
    let original = callback_error.clone();
    let retained_host = Rc::new(RefCell::new(None));
    let retained = retained_host.clone();
    let returned = tembra::native::run("dx-native-error", (), move |host| async move {
        *retained.borrow_mut() = Some(host.clone());
        let events = setup_events.clone();
        host.serve(Dependency, move |_, _, declaration| {
            let close_events = events.clone();
            let resource = ResourceDeclaration::new(|_| async { Ok(1_u64) }.boxed_local())
                .with_close(move |_| {
                    let events = close_events.clone();
                    async move {
                        events.borrow_mut().push("close attempted");
                        Err(RuntimeError::unavailable("cleanup", "test closer failed"))
                    }
                    .boxed_local()
                });
            Ok(DependencyService {
                value: declaration.core_mut().resource("value", resource)?,
            })
        })?;
        let dependency = host.get(Dependency, DependencyInput {}).await?;
        assert_eq!(dependency.read().await?, 1);
        setup_events.borrow_mut().push("callback refused");
        Err::<(), _>(callback_error)
    });
    let returned = returned.unwrap_err();
    assert_eq!(returned.code, original.code);
    assert_eq!(returned.reason, original.reason);
    assert_eq!(returned.message, original.message);
    assert_eq!(returned.details()["callbackDetails"], *original.details());
    assert_eq!(returned.details()["cleanup"]["reason"], "cleanup");
    assert_eq!(*events.borrow(), ["callback refused", "close attempted"]);
    let retained = retained_host.borrow_mut().take().unwrap();
    // Shutdown has physically settled. Reawaiting the shared result needs no new runtime.
    let cleanup = futures::executor::block_on(retained.close()).unwrap_err();
    assert_eq!(cleanup.reason, "cleanup");
    Ok(())
}
