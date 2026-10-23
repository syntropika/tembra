use super::*;
use deno_core::{OpState, op2};
use std::time::Duration;
use tembra_native::TokioDriver;

struct TestGate(Option<oneshot::Receiver<()>>);
#[op2]
async fn op_test_gate(state: Rc<RefCell<OpState>>) {
    let receive = state
        .borrow_mut()
        .borrow_mut::<TestGate>()
        .0
        .take()
        .unwrap();
    receive.await.unwrap();
}
deno_core::extension!(test_ops, ops = [op_test_gate]);

#[test]
fn pending_evaluation_owns_engine_after_executor_and_waiter_are_dropped() {
    let driver = TokioDriver::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(driver.run_until(async {
        let scenario = async {
            let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
            let host = tembra_core::Host::new(
                tembra_core::HostOptions::new("observed-prepare", serde_json::json!(null))
                    .with_observability(tembra_core::ObservationOptions::new(64, 1).unwrap()),
                driver.clone(),
            );
            let observations = host.observations().unwrap();
            let (send, receive) = oneshot::channel();
            executor
                .inner
                .runtime
                .borrow()
                .op_state()
                .borrow_mut()
                .put(TestGate(Some(receive)));
            let source = r#"
                await globalThis.__tembraTestGate();
                export default { name:'pending', uses:[], setup(){} };
            "#;
            let artifact =
                BundledJsArtifact::new("pending", source, vec!["default".into()]).unwrap();
            let waiter = executor.prepare_for_host(&host, artifact, "default".into());
            while executor.stats().evaluations == 0 {
                tokio::task::yield_now().await;
            }
            let begun = observations.drain(64);
            let operation = begun
                .iter()
                .find(|event| event.label == "module_prepare" && event.outcome.is_none())
                .unwrap()
                .id;
            let weak = Rc::downgrade(&executor.inner);
            drop(waiter);
            drop(executor);
            tokio::task::yield_now().await;
            assert!(
                weak.upgrade().is_some(),
                "physical evaluation retains its isolate"
            );
            assert_eq!(
                Executor::new(driver.clone(), ExecutorOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "engine_thread",
            );
            assert!(
                !observations
                    .drain(64)
                    .iter()
                    .any(|event| event.id == operation && event.outcome.is_some())
            );
            send.send(()).unwrap();
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
            assert!(
                observations
                    .drain(64)
                    .iter()
                    .any(|event| event.id == operation
                        && event.outcome == Some(tembra_core::observability::Outcome::Ok))
            );
            host.close().await.unwrap();
            drop(Executor::new(driver.clone(), ExecutorOptions::default()).unwrap());
        };
        tokio::time::timeout(Duration::from_secs(10), scenario)
            .await
            .unwrap();
    }));
}

#[test]
fn observed_preparation_refusal_does_not_retain_host_or_fake_evaluation() {
    let driver = TokioDriver::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(driver.run_until(async {
        let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
        let host = tembra_core::Host::new(
            tembra_core::HostOptions::new("closed-prepare", serde_json::json!(null))
                .with_observability(tembra_core::ObservationOptions::new(64, 1).unwrap()),
            driver.clone(),
        );
        let observations = host.observations().unwrap();
        executor.inner.close_request_channel();
        let artifact = BundledJsArtifact::new(
            "refused",
            "export default {name:'refused',uses:[],setup(){}}",
            vec!["default".into()],
        )
        .unwrap();
        assert!(
            executor
                .prepare_for_host(&host, artifact, "default".into())
                .await
                .is_err()
        );
        assert_eq!(executor.stats().evaluations, 0);
        assert!(observations.drain(64).iter().any(|event| matches!(
            event.outcome,
            Some(tembra_core::observability::Outcome::Error { .. })
        )));
        host.close().await.unwrap();
    }));
}
