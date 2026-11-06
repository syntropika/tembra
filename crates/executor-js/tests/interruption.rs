use futures::FutureExt;
use std::{cell::Cell, rc::Rc};
use std::{thread, time::Duration};
use tembra::Host;
use tembra::{Orb, Service};
use tembra_executor_js::{CooperativeExecutor, Runtime};

fn sdk_source() -> std::io::Result<String> {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/ts/sdk/dist/embedded.js"
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn a_watchdog_can_interrupt_synchronous_evaluation_and_callbacks()
-> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (source, cancelled) in [
                ("while (true) {}", false),
                ("(async () => { await host.install({name:'loop',apiVersion:1,services:{loop:{methods:{run(){while(true){}}}}}}); const reference=await host.get('loop','main'); return reference.run(); })()", true),
            ] {
                let executor = CooperativeExecutor::new();
                let host = Host::new(executor.clone());
                let mut runtime = Runtime::with_driver(host, sdk_source()?, executor)?;
                let interrupt = runtime.interrupt_handle();
                let watchdog = thread::spawn(move || {
                    thread::sleep(Duration::from_millis(100));
                    interrupt.terminate()
                });
                let result = runtime.evaluate(source).await;
                assert!(watchdog.join().map_err(|_| "watchdog panicked")?);
                let error = result.err().ok_or("execution was not interrupted")?;
                assert!(error.message.contains("execution terminated"), "{error}");
                let closing = tokio::time::timeout(Duration::from_secs(2), runtime.close()).await?;
                if cancelled {
                    assert!(closing.unwrap_err().message.contains("operation was cancelled"));
                } else {
                    closing?;
                }
                let diagnostics = runtime.native().kernel().diagnostics();
                assert_eq!((diagnostics.objects, diagnostics.calls, diagnostics.resources), (0,0,0));
            }
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_interruption_closes_execution_admission() -> Result<(), Box<dyn std::error::Error>>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let executor = CooperativeExecutor::new();
            let host = Host::new(executor.clone());
            let mut runtime = Runtime::with_driver(host, sdk_source()?, executor)?;
            let interrupt = runtime.interrupt_handle();
            assert!(
                thread::spawn(move || interrupt.terminate())
                    .join()
                    .map_err(|_| "watchdog panicked")?
            );
            let error = runtime
                .evaluate("42")
                .await
                .err()
                .ok_or("an idle interruption was lost")?;
            assert_eq!(error.code, tembra::Code::Closed);
            runtime.close().await?;
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn interrupt_handles_do_not_keep_the_isolate_alive() -> Result<(), Box<dyn std::error::Error>>
{
    tokio::task::LocalSet::new()
        .run_until(async {
            let executor = CooperativeExecutor::new();
            let host = Host::new(executor.clone());
            let mut runtime = Runtime::with_driver(host, sdk_source()?, executor)?;
            let interrupt = runtime.interrupt_handle();
            runtime.close().await?;
            drop(runtime);
            assert!(!interrupt.terminate());
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_interrupted_realm_can_finish_host_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let executor = CooperativeExecutor::new();
            let host = Host::new(executor.clone());
            let mut runtime = Runtime::with_driver(host.clone(), sdk_source()?, executor)?;
            let interrupt = runtime.interrupt_handle();
            let watchdog = thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                interrupt.terminate()
            });
            let error = runtime.evaluate("while (true) {}").await.unwrap_err();
            assert!(watchdog.join().map_err(|_| "watchdog panicked")?);
            assert!(error.message.contains("execution terminated"));
            tokio::time::timeout(Duration::from_secs(2), runtime.close()).await??;
            assert!(host.kernel().diagnostics().closing);
            assert_eq!(host.kernel().diagnostics().objects, 0);
            assert_eq!(host.kernel().diagnostics().resources, 0);
            let error = runtime.evaluate("42").await.unwrap_err();
            assert_eq!(error.code, tembra::Code::Closed);
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_callbacks_release_mixed_resources_without_repeating_effects()
-> Result<(), Box<dyn std::error::Error>> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let executor = CooperativeExecutor::new();
            let host = Host::new(executor.clone());
            let effects = Rc::new(Cell::new(0));
            let closed = Rc::new(Cell::new(0));
            let effects_factory = effects.clone();
            let closed_factory = closed.clone();
            host.install(vec![
                Orb::new("native", 1).service(
                    Service::new("native", move |declaration| {
                        let closed = closed_factory.clone();
                        declaration.resource(
                            "native-resource",
                            1,
                            || Ok(()),
                            move |_, _| {
                                let closed = closed.clone();
                                async move {
                                    closed.set(closed.get() + 1);
                                    Ok(())
                                }
                                .boxed_local()
                            },
                        )?;
                        Ok(effects_factory.clone())
                    })
                    .method(
                        "effect",
                        |effects, _, _: [serde_json::Value; 0]| async move {
                            effects.set(effects.get() + 1);
                            Ok(())
                        },
                    ),
                ),
            ])?;
            let mut runtime = Runtime::with_driver(host.clone(), sdk_source()?, executor)?;
            runtime
                .evaluate(
                    r#"(async () => {
            await host.install({name:'js',apiVersion:1,
                requires:{native:{apiVersion:1,methods:['effect']}},
                services:{loop:{create(self){
                    const native = self.get('native','main');
                    self.resource('js-resource',{deps:[native],open:()=>null,
                        close:(_value,ctx)=>native.effect(ctx)});
                    return native;
                },methods:{async run(native,ctx){await native.effect(ctx);while(true){}}}}}});
            globalThis.reference = await host.get('loop','main');
        })()"#,
                )
                .await?;
            let interrupt = runtime.interrupt_handle();
            let watchdog = thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                interrupt.terminate()
            });
            let result = runtime.evaluate("reference.run()").await;
            assert!(watchdog.join().map_err(|_| "watchdog panicked")?);
            assert!(result.unwrap_err().message.contains("execution terminated"));
            assert_eq!(effects.get(), 1);
            let closing = tokio::time::timeout(Duration::from_secs(2), runtime.close()).await;
            closing??;
            assert_eq!(effects.get(), 2);
            assert_eq!(closed.get(), 1);
            let diagnostics = host.kernel().diagnostics();
            assert_eq!(
                (
                    diagnostics.objects,
                    diagnostics.calls,
                    diagnostics.resources,
                    diagnostics.tasks
                ),
                (0, 0, 0, 0)
            );
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_shutdown_waits_for_native_physical_completion()
-> Result<(), Box<dyn std::error::Error>> {
    use futures::channel::oneshot;
    use std::cell::RefCell;
    tokio::task::LocalSet::new()
        .run_until(async {
            let executor = CooperativeExecutor::new();
            let host = Host::new(executor.clone());
            let (send, receive) = oneshot::channel::<()>();
            let receive = Rc::new(RefCell::new(Some(receive)));
            let started = Rc::new(Cell::new(false));
            let finished = Rc::new(Cell::new(false));
            let closed = Rc::new(Cell::new(false));
            let factory_started = started.clone();
            let factory_finished = finished.clone();
            let factory_closed = closed.clone();
            host.install(vec![
                Orb::new("held", 1).service(
                    Service::new("held", move |declaration| {
                        let closed = factory_closed.clone();
                        let finished = factory_finished.clone();
                        declaration.resource(
                            "held-resource",
                            1,
                            || Ok(()),
                            move |_, _| {
                                let closed = closed.clone();
                                let finished = finished.clone();
                                async move {
                                    assert!(finished.get());
                                    closed.set(true);
                                    Ok(())
                                }
                                .boxed_local()
                            },
                        )?;
                        Ok((
                            receive.clone(),
                            factory_started.clone(),
                            factory_finished.clone(),
                        ))
                    })
                    .method(
                        "wait",
                        |state, _, _: [serde_json::Value; 0]| async move {
                            let receive = state.0.borrow_mut().take().unwrap();
                            state.1.set(true);
                            receive.await.unwrap();
                            state.2.set(true);
                            Ok(42)
                        },
                    ),
                ),
            ])?;
            let mut runtime = Runtime::with_driver(host.clone(), sdk_source()?, executor)?;
            runtime
                .evaluate("(async()=>{globalThis.held=await host.get('held','main');})()")
                .await?;
            assert!(
                tokio::time::timeout(Duration::from_millis(10), runtime.evaluate("held.wait()"))
                    .await
                    .is_err()
            );
            assert!(started.get());
            let interrupt = runtime.interrupt_handle();
            let watchdog = thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                interrupt.terminate()
            });
            assert!(runtime.evaluate("while(true){}").await.is_err());
            assert!(watchdog.join().map_err(|_| "watchdog panicked")?);
            let pending_host = host.clone();
            let pending_closed = closed.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert!(!pending_closed.get());
                assert_eq!(pending_host.kernel().diagnostics().calls, 1);
                send.send(()).unwrap();
            });
            tokio::time::timeout(Duration::from_secs(2), runtime.close()).await??;
            assert!(finished.get());
            assert!(closed.get());
            assert_eq!(host.kernel().diagnostics().calls, 0);
            Ok(())
        })
        .await
}
