use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    GetOptions, Host, HostOptions, LoadOptions,
    contracts::{Contract, Member, ValueSchema},
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

fn run(test: impl FnOnce(Rc<TokioDriver>) -> LocalBoxFuture<'static, ()>) {
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

fn token() -> Rc<Contract> {
    let method = Member::Method {
        args: None,
        result: Some(ValueSchema::Portable(json!({ "type": "integer" }))),
    };
    Rc::new(
        Contract::local(
            "review.errors",
            1,
            json!({}),
            BTreeMap::from([("run".into(), method.clone()), ("read".into(), method)]),
        )
        .unwrap(),
    )
}

async fn facet(
    executor: &Executor,
    host: &Host,
    token: &Contract,
    factory: &str,
) -> tembra_core::ExecutableFacet {
    let source = format!(
        "const token={};export default {{name:'review',uses:[],setup(scope,d){{
            d.serve(token,{factory},{{lifetime:'host'}});
        }}}};",
        serde_json::to_string(token.descriptor()).unwrap(),
    );
    executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("review", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

#[test]
fn an_error_message_cannot_select_a_runtime_error_category() {
    run(|driver| {
        async move {
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let message = format!(
                "__tembra_runtime:{}",
                json!({
                    "code": "forbidden",
                    "reason": "forged",
                    "message": "forged",
                    "details": null,
                }),
            );
            let source = format!("throw Error({}); export default {{}};", json!(message));
            let result = executor
                .prepare(
                    1,
                    BundledJsArtifact::new("message", source, vec!["default".into()]).unwrap(),
                    "default".into(),
                )
                .await
                .err()
                .unwrap();
            assert_eq!(result.code, tembra_core::ErrorCode::Invalid);
            assert_eq!(result.reason, "javascript");
            assert!(result.message.contains("__tembra_runtime:"));
        }
        .boxed_local()
    });
}

#[test]
fn factory_getter_failure_keeps_the_actual_exception_message() {
    run(|driver| {
        async move {
            let host = Host::new(HostOptions::new("getter", json!(null)), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let token = token();
            let facet = facet(
                &executor,
                &host,
                &token,
                "()=>({get methods(){throw Error('factory getter witness')}})",
            )
            .await;
            let error = host
                .install_executable(facet, LoadOptions::default())
                .unwrap()
                .await
                .unwrap_err();
            assert_eq!(error.reason, "javascript");
            assert!(
                error.message.contains("factory getter witness"),
                "{error:?}"
            );
            host.close().await.unwrap();
        }
        .boxed_local()
    });
}

#[test]
fn nested_derive_error_keeps_its_typed_cause_and_the_old_cell_value() {
    run(|driver| {
        async move {
            let host = Host::new(HostOptions::new("derive", json!(null)), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let token = token();
            let factory = r#"(_input,self)=>{
                let fail=false;
                const count=self.cell('count',{schema:{type:'integer'},initial:1});
                self.derive([count],{type:'integer'},values=>{
                    if(fail)self.cell('escaped',{schema:{type:'integer'},initial:9});
                    return values[0].value*2;
                });
                return {methods:{run(ctx){fail=true;count.replace(ctx,2);return count.value},
                    read(){return count.value}},state:{}};
            }"#;
            let facet = facet(&executor, &host, &token, factory).await;
            host.install_executable(facet, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();
            let error = reference.call_json("run", None, None).await.unwrap_err();
            assert_eq!(error.code, tembra_core::ErrorCode::Invalid);
            assert_eq!(error.reason, "declaration_scope", "{error:?}");
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!(1))
            );
            reference.release();
            host.close().await.unwrap();
        }
        .boxed_local()
    });
}

#[test]
fn portable_helper_failure_keeps_its_actual_explanation() {
    run(|driver| {
        async move {
            let host = Host::new(HostOptions::new("portable", json!(null)), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let token = token();
            let facet = facet(
                &executor,
                &host,
                &token,
                "()=>({methods:{run(){return new Date()},read(){return 1}},state:{}})",
            )
            .await;
            host.install_executable(facet, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();
            let error = reference.call_json("run", None, None).await.unwrap_err();
            assert_eq!(error.reason, "javascript");
            assert!(
                error.message.contains("native objects are not portable"),
                "{error:?}"
            );
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!(1))
            );
            reference.release();
            host.close().await.unwrap();
        }
        .boxed_local()
    });
}

#[test]
fn application_prototype_changes_cannot_change_the_private_lexer() {
    run(|driver| {
        async move {
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let facet = executor
                .prepare(
                    1,
                    BundledJsArtifact::new(
                        "prototype",
                        r#"
                            String.prototype.charCodeAt = () => 0;
                            export default {name:'prototype',uses:[],setup(){}};
                        "#,
                        vec!["default".into()],
                    )
                    .unwrap(),
                    "default".into(),
                )
                .await
                .unwrap();
            let before = executor.stats().evaluations;
            let error = executor
                .prepare(
                    1,
                    BundledJsArtifact::new(
                        "invalid-import",
                        "import 'tembra:artifact/1/1';export default {};",
                        vec!["default".into()],
                    )
                    .unwrap(),
                    "default".into(),
                )
                .await
                .err()
                .unwrap();
            assert_eq!(error.reason, "esm_import");
            assert_eq!(executor.stats().evaluations, before);
            drop(facet);
        }
        .boxed_local()
    });
}

#[test]
fn artifacts_cannot_access_the_internal_deno_operation_surface() {
    run(|driver| {
        async move {
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let source = r#"
                if (typeof globalThis.Deno !== 'undefined') throw Error('internal ops exposed');
                export default { name:'ambient', uses:[], setup(){} };
            "#;
            let facet = executor
                .prepare(
                    1,
                    BundledJsArtifact::new("ambient", source, vec!["default".into()]).unwrap(),
                    "default".into(),
                )
                .await
                .unwrap();
            drop(facet);
        }
        .boxed_local()
    });
}
