use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
fn run(test: impl FnOnce(Rc<TokioDriver>) -> futures::future::LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), test(driver.clone()))
                .await
                .unwrap();
        }));
}

fn contract(args: Option<ValueSchema>, result: Option<ValueSchema>) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "boundary",
            1,
            json!({}),
            BTreeMap::from([("run".into(), Member::Method { args, result })]),
        )
        .unwrap(),
    )
}

async fn install(host: &Host, executor: &Executor, token: &Contract, body: &str) {
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'boundary', uses: [], setup(scope, d) {{
        d.serve(token, () => ({{
            methods: {{
                run: async function (ctx, arg) {{
                    {body};
                }}
            }}, state: {{}}
        }}));
    }}
}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    let artifact = BundledJsArtifact::new("boundary", source, vec!["default".into()]).unwrap();
    let facet = executor
        .prepare(host.id(), artifact, "default".into())
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
}

#[test]
fn unused_context_signal_waiter_physically_settles_when_its_scope_ends() {
    run(|driver| check_unused_signal(driver).boxed_local());
}

#[test]
fn portable_void_discards_result_without_conversion_after_promise_settlement() {
    for body in [
        "return undefined;",
        "return null;",
        "return 123;",
        "return Promise.resolve(123);",
        "return ()=>123;",
        "return {get secret(){throw Error('discarded getter ran');}};",
    ] {
        run(move |driver| {
            async move {
                let host = Host::new(HostOptions::new("void", json!(null)), driver.clone());
                let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
                let token = contract(None, None);
                install(&host, &executor, &token, body).await;
                let reference = host
                    .get(token, json!({}), GetOptions::default())
                    .await
                    .unwrap();
                let result = reference.call_json("run", None, None).await;
                assert_eq!(result.unwrap(), None);
                reference.release();
                host.close().await.unwrap();
                assert_eq!(executor.stats().physical_jobs, 0);
            }
            .boxed_local()
        });
    }
}

struct Marker;
struct MarkerCodec;
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}

struct PermissiveUnit;
impl ScopedCodec<()> for PermissiveUnit {
    fn encode(&self, (): ()) -> Result<NativeValue> {
        panic!("absent argument codec must not run")
    }

    fn decode(&self, _: NativeValue) -> Result<()> {
        panic!("void result codec must not run")
    }
}

#[test]
fn native_void_discards_result_without_calling_unit_codec() {
    for body in [
        "return undefined;",
        "return null;",
        "return 123;",
        "return ()=>123;",
        "return {get secret(){throw Error('discarded getter ran');}};",
    ] {
        run(move |driver| {
            async move {
                let host = Host::new(HostOptions::new("native-void", json!(null)), driver.clone());
                let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
                executor
                    .register_native_method(
                        NativeMethodSignature::new(Some("marker@1".into()), None).unwrap(),
                        NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(PermissiveUnit)),
                    )
                    .unwrap();
                let token = contract(Some(ValueSchema::Native("marker@1".into())), None);
                install(&host, &executor, &token, body).await;
                let reference = host
                    .get(token, json!({}), GetOptions::default())
                    .await
                    .unwrap();
                let result = reference
                    .call::<Rc<Marker>, ()>("run", Rc::new(Marker), None)
                    .await;
                result.unwrap();
                reference.release();
                host.close().await.unwrap();
            }
            .boxed_local()
        });
    }
}

#[test]
fn method_throw_and_detached_rejection_are_observed_without_stranding_jobs() {
    for body in [
        "throw Error('method failure');",
        "Promise.reject(Error('detached rejection'));return 1;",
    ] {
        run(move |driver| {
            async move {
                let host = Host::new(HostOptions::new("errors", json!(null)), driver.clone());
                let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
                let token = contract(
                    None,
                    Some(ValueSchema::Portable(json!({ "type": "integer" }))),
                );
                install(&host, &executor, &token, body).await;
                let reference = host
                    .get(token, json!({}), GetOptions::default())
                    .await
                    .unwrap();
                let result = reference.call_json("run", None, None).await;
                if body.starts_with("throw") {
                    assert!(result.unwrap_err().message.contains("method failure"));
                } else {
                    assert_eq!(result.unwrap(), Some(json!(1)));
                    tokio::task::yield_now().await;
                    let errors = executor.take_errors();
                    assert!(
                        errors
                            .iter()
                            .any(|error| error.message.contains("detached rejection")),
                        "{errors:?}"
                    );
                    assert!(executor.take_errors().is_empty());
                }

                reference.release();
                host.close().await.unwrap();
                assert_eq!(executor.stats().physical_jobs, 0);
                assert_eq!(executor.stats().contexts, 0);
            }
            .boxed_local()
        });
    }
}

#[test]
fn unmapped_native_signature_and_callback_quota_refuse_activation_or_setup() {
    run(|driver| check_adapter_limits(driver).boxed_local());
}

#[test]
fn native_absent_arguments_bypass_codec_and_send_only_context() {
    run(|driver| check_absent_arguments(driver).boxed_local());
}

async fn check_unused_signal(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("signal", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let token = contract(
        None,
        Some(ValueSchema::Portable(json!({ "type": "integer" }))),
    );
    install(
        &host,
        &executor,
        &token,
        "void ctx.signal.cancelled; return 1;",
    )
    .await;
    let reference = host
        .get(token, json!({}), GetOptions::default())
        .await
        .unwrap();
    for _ in 0..140 {
        assert_eq!(
            reference.call_json("run", None, None).await.unwrap(),
            Some(json!(1))
        );
        tokio::task::yield_now().await;
        assert_eq!(executor.stats().physical_jobs, 0);
        assert_eq!(executor.stats().contexts, 0);
    }

    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}

async fn check_adapter_limits(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("unmapped", json!(null)), driver.clone());
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let token = contract(Some(ValueSchema::Native("unmapped@1".into())), None);
    install(
        &host,
        &executor,
        &token,
        "throw Error('handler must not be entered');",
    )
    .await;
    assert_eq!(
        host.get(token, json!({}), GetOptions::default())
            .await
            .err()
            .unwrap()
            .reason,
        "native_signature"
    );
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
    drop(executor);
    let host = Host::new(
        HostOptions::new("callback-limit", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(
        driver,
        ExecutorOptions {
            max_callbacks: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let token = contract(None, None);
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'quota', uses: [], setup(s, d) {{
        d.serve(token, () => ({{
            methods: {{
                run() {{
                }}
            }}, state: {{}}
        }}));
    }}
}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("quota", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        host.install_executable(facet, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "callbacks"
    );
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
    assert_eq!(executor.stats().contexts, 0);
}

async fn check_absent_arguments(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("no-args", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(None, Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(PermissiveUnit), Rc::new(MarkerCodec)),
        )
        .unwrap();
    let token = contract(None, Some(ValueSchema::Native("marker@1".into())));
    install(
        &host,
        &executor,
        &token,
        "if(arguments.length!==1)throw Error('synthetic argument');throw Error('actual noargs');",
    )
    .await;
    let reference = host
        .get(token, json!({}), GetOptions::default())
        .await
        .unwrap();
    let error = reference
        .call::<(), Rc<Marker>>("run", (), None)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("actual noargs"));
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}
