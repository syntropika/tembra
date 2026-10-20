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
            let waiter = executor.prepare(1, artifact, "default".into());
            while executor.stats().evaluations == 0 {
                tokio::task::yield_now().await;
            }
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
            send.send(()).unwrap();
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
            drop(Executor::new(driver.clone(), ExecutorOptions::default()).unwrap());
        };
        tokio::time::timeout(Duration::from_secs(10), scenario)
            .await
            .unwrap();
    }));
}
