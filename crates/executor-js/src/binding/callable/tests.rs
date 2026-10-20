use super::*;
use crate::{
    Executor, ExecutorOptions, NativeCall, NativeCallable, NativeMethodCodec,
    NativeMethodSignature, NativeValue, ScopedCodec,
};
use std::{cell::Cell, collections::BTreeMap, time::Duration};
use tembra_native::TokioDriver;

struct Probe {
    engine: Weak<Engine>,
    drops: Rc<Cell<usize>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        let engine = self.engine.upgrade().unwrap();
        engine
            .with_scope(|scope| {
                let value = v8::Number::new(scope, 7.0);
                assert_eq!(value.value(), 7.0);
                Ok(())
            })
            .unwrap();
        self.drops.set(self.drops.get() + 1);
    }
}
struct ClosedChannel {
    engine: Weak<Engine>,
    drops: Rc<Cell<usize>>,
    completed: Rc<Cell<usize>>,
}
impl ScopedCodec<()> for ClosedChannel {
    fn encode(&self, (): ()) -> Result<NativeValue> {
        let engine = self.engine.clone();
        let drops = self.drops.clone();
        let completed = self.completed.clone();
        Ok(NativeValue::callable(NativeCallable::deferred(Rc::new(
            move |decode| {
                engine.upgrade().unwrap().close_request_channel();
                let probe = Probe {
                    engine: engine.clone(),
                    drops: drops.clone(),
                };
                let engine = engine.clone();
                let completed = completed.clone();
                NativeCall::with_completion(
                    async move {
                        let _probe = probe;
                        decode()
                    }
                    .boxed_local(),
                    Box::new(move |projection| {
                        assert!(
                            projection.is_ok(),
                            "prepared rejection is not projection failure"
                        );
                        engine.upgrade().unwrap().with_scope(|_| Ok(())).unwrap();
                        completed.set(completed.get() + 1);
                    }),
                )
            },
        ))))
    }
    fn decode(&self, _: NativeValue) -> Result<()> {
        unreachable!("argument-only codec")
    }
}
struct Integer;
impl ScopedCodec<i64> for Integer {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        Ok(NativeValue::portable(json!(value)))
    }
    fn decode(&self, value: NativeValue) -> Result<i64> {
        Ok(value.into_portable()?.as_i64().unwrap())
    }
}

#[test]
fn failed_queue_send_rejects_promise_and_drops_owned_work_after_runtime_borrow() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(10), scenario(driver.clone()))
                .await
                .unwrap();
        }));
}
async fn scenario(driver: Rc<dyn Driver>) {
    let host = Host::new(HostOptions::new("closed-send", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let drops = Rc::new(Cell::new(0));
    let completed = Rc::new(Cell::new(0));
    let input = ValueSchema::Native("closed-send@1".into());
    let output = ValueSchema::Portable(json!({"type":"integer"}));
    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(input.clone()), Some(output.clone())).unwrap(),
            NativeMethodCodec::new(
                Rc::new(ClosedChannel {
                    engine: Rc::downgrade(&executor.inner),
                    drops: drops.clone(),
                    completed: completed.clone(),
                }),
                Rc::new(Integer),
            ),
        )
        .unwrap();
    let token = Rc::new(
        Contract::local(
            "closed-send",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: Some(input),
                    result: Some(output),
                },
            )]),
        )
        .unwrap(),
    );
    let source = format!(
        r#"
const token = {};
export default {{name: 'closed-send', uses: [], setup(scope, declare) {{
    declare.serve(token, () => ({{methods: {{async run(ctx, callback) {{
        let pending;
        try {{ pending = callback(1); }}
        catch (_) {{ throw Error('callable refusal threw synchronously'); }}
        if (!(pending instanceof Promise)) throw Error('Promise required');
        try {{ await pending; }}
        catch (error) {{
            if (error.reason !== 'executor_closed') throw error;
            return 4;
        }}
        throw Error('closed queue accepted work');
    }}}}, state: {{}}}}));
}}}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    let facet = executor
        .prepare(
            host.id(),
            crate::BundledJsArtifact::new("closed-send", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let held = host
        .get(token, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(held.call::<(), i64>("run", (), None).await.unwrap(), 4);
    assert_eq!(drops.get(), 1);
    assert_eq!(completed.get(), 1);
    held.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().queued_jobs, 0);
}

#[test]
fn failed_queue_send_progresses_with_manual_driver_after_pump_is_idle() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let driver = ManualDriver::new();
    driver.run_until(scenario(driver.clone()));
}
