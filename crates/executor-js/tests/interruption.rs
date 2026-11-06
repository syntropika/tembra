use std::{thread, time::Duration};
use tembra::Host;
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
            for source in [
                "while (true) {}",
                "(async () => { await host.install({name:'loop',apiVersion:1,services:{loop:{methods:{run(){while(true){}}}}}}); const reference=await host.get('loop','main'); return reference.run(); })()",
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
                // Termination is not evidence that a shared host has physically closed.
                drop(runtime);
            }
            Ok(())
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_interruption_applies_to_the_next_execution()
-> Result<(), Box<dyn std::error::Error>> {
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
            assert!(error.message.contains("execution terminated"), "{error}");
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
