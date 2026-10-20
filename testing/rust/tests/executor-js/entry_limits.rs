//! Unentered initializer/opener roots legitimately invoke actual managed dependencies.
use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
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
fn initializer_context_can_start_a_native_dependency_before_its_callback_settles() {
    run(false);
}
#[test]
fn opener_explicit_facet_root_can_start_a_native_dependency() {
    run(true);
}
fn run(opener: bool) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(10), scenario(driver.clone(), opener))
                .await
                .unwrap();
        }));
}
async fn scenario(driver: Rc<TokioDriver>, opener: bool) {
    let host = Host::new(HostOptions::new("entry-probe", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let argument = ValueSchema::Native("probe-i64@1".into());
    let result = ValueSchema::Portable(json!({"type":"integer"}));
    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(argument.clone()), Some(result.clone()))
                .unwrap(),
            NativeMethodCodec::new(Rc::new(Integer), Rc::new(Integer)),
        )
        .unwrap();
    let native = Rc::new(
        Contract::local(
            "native-probe",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "wait".into(),
                    Member::Method {
                        args: Some(argument),
                        result: Some(result),
                    },
                ),
                (
                    "tick".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let entered = effects.clone();
    let (release, wait) = oneshot::channel();
    let wait = Rc::new(RefCell::new(Some(wait)));
    host.serve(
        native.clone(),
        Rc::new(move |_, _, _| {
            let effects = entered.clone();
            let wait = wait.clone();
            Ok(Implementation::new()
                .method("wait", move |value: i64, _| {
                    effects.set(effects.get() + 1);
                    let wait = wait.borrow_mut().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Ok(value)
                    }
                    .boxed_local()
                })
                .json_method("tick", |_, _| async { Ok(None) }.boxed_local()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let service = Rc::new(
        Contract::local(
            "entry-service",
            1,
            json!({}),
            BTreeMap::from([(
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            )]),
        )
        .unwrap(),
    );
    let observer = Rc::new(
        Contract::local(
            "entry-observer",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                },
            )]),
        )
        .unwrap(),
    );
    let declaration = if opener {
        "const count=self.cell('count',{schema:{type:'integer'},initial:1});\n\
         self.resource('held',{deps:[resource],open:()=>scope.root(ctx=>work(ctx))});"
    } else {
        "const count=self.cell('count',{schema:{type:'integer'},initialize:ctx=>work(ctx)});"
    };
    let source = format!(
        r#"
const native = {}, service = {}, observer = {};
export default {{name: 'entry-probe', uses: [native], setup(scope, declare) {{
    declare.serve(observer, () => ({{methods: {{
        read: () => [globalThis.closes ?? 0, globalThis.lateUse ?? null]
    }}, state: {{}}}}));
    declare.serve(service, (input, self) => {{
        const dependency = self.get(native, {{}});
        const resource = self.resource('raw', {{
            open: () => ({{closed: false, read() {{
                if (this.closed) throw Error('raw resource already closed');
                return 7;
            }}}}),
            close(value) {{value.closed = true; globalThis.closes = (globalThis.closes ?? 0) + 1;}}
        }});
        async function work(ctx) {{
            const raw = resource.value;
            void dependency.wait(4, ctx).then(() => {{
                try {{globalThis.lateUse = raw.read();}}
                catch (_) {{globalThis.lateUse = 'closed';}}
            }}, error => {{globalThis.lateUse = error.reason;}});
            // A real separate core callback yields the driver, so an admitted native
            // effect can start before the initializer/opener itself settles.
            await dependency.tick(ctx);
            return 1;
        }}
        {declaration}
        return {{methods: {{}}, state: {{count}}}};
    }});
}}}};
"#,
        serde_json::to_string(native.descriptor()).unwrap(),
        serde_json::to_string(service.descriptor()).unwrap(),
        serde_json::to_string(observer.descriptor()).unwrap()
    );
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("entry-probe", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(service, json!({}), GetOptions::default())
        .await
        .unwrap();
    let observed_effects = effects.get();
    let observer = host
        .get(observer, json!({}), GetOptions::default())
        .await
        .unwrap();
    reference.release();
    let before = tokio::time::timeout(Duration::from_millis(30), async {
        loop {
            let value = observer
                .call_json("read", None, None)
                .await
                .unwrap()
                .unwrap();
            if value[0] == json!(1) {
                break value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let _ = release.send(());
    let after = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = observer
                .call_json("read", None, None)
                .await
                .unwrap()
                .unwrap();
            if value[1] != serde_json::Value::Null {
                break value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    eprintln!("raw alias ownership: opener={opener}, before={before:?}, after={after:?}");
    observer.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(
        observed_effects, 1,
        "actual dependency did not enter; opener={opener}"
    );
}
