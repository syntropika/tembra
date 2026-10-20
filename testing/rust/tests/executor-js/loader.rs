use futures::FutureExt;
use serde_json::{Value, json};
use std::{rc::Rc, time::Duration};
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
#[test]
fn exact_shared_lexical_corpus_is_checked_before_evaluation() {
    run(|driver| {
        async move {
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let corpus: Value =
                serde_json::from_str(include_str!("../../../fixtures/executor/esm-v1.json"))
                    .unwrap();
            assert_eq!(corpus["cases"].as_array().unwrap().len(), 15);
            for case in corpus["cases"].as_array().unwrap() {
                let source = format!(
                    "{};export const __probe={{name:'probe',uses:[],setup(){{}}}};",
                    case["source"].as_str().unwrap()
                );
                let mut exports: Vec<String> =
                    serde_json::from_value(case["exports"].clone()).unwrap();
                exports.push("__probe".into());
                let artifact =
                    BundledJsArtifact::new(case["id"].as_str().unwrap(), source, exports).unwrap();
                let before = executor.stats().evaluations;
                let result = executor.prepare(1, artifact, "__probe".into()).await;
                if case["accepted"] == true {
                    assert!(result.is_ok(), "{}: {:?}", case["id"], result.err());
                    assert_eq!(executor.stats().evaluations, before + 1);
                } else {
                    assert!(result.is_err(), "{}", case["id"]);
                    assert_eq!(executor.stats().evaluations, before);
                }
            }
        }
        .boxed_local()
    });
}
#[test]
fn integrity_export_and_failed_admitted_evaluation_budgets_are_distinct() {
    run(|driver| {
        async move {
            let executor = Executor::new(
                driver,
                ExecutorOptions {
                    max_evaluations: 3,
                    ..Default::default()
                },
            )
            .unwrap();
            let source = "export default {name:'probe',uses:[],setup(){}}";
            let artifact = BundledJsArtifact::new("probe", source, vec!["default".into()]).unwrap();
            let envelope = json!(
                { "format" : "tembra-esm-v1", "identity" : "probe", "source" : source,
                "sha256" : artifact.sha256(), "exports" : ["default"] }
            );
            assert_eq!(
                BundledJsArtifact::from_json(&envelope.to_string())
                    .unwrap()
                    .sha256(),
                artifact.sha256()
            );
            let mut corrupt = envelope;
            corrupt["sha256"] = json!("0".repeat(64));
            assert_eq!(
                BundledJsArtifact::from_json(&corrupt.to_string())
                    .unwrap_err()
                    .reason,
                "esm_artifact"
            );
            assert!(
                executor
                    .prepare(1, artifact.clone(), "missing".into())
                    .await
                    .is_err()
            );
            assert_eq!(executor.stats().evaluations, 0);
            let mismatch =
                BundledJsArtifact::new("mismatch", source, vec!["default".into(), "other".into()])
                    .unwrap();
            assert!(
                executor
                    .prepare(1, mismatch, "default".into())
                    .await
                    .is_err()
            );
            assert_eq!(executor.stats().evaluations, 0);
            for source in [
                "export const typed:number=1;export default {};",
                "throw Error('top-level');export default {};",
            ] {
                let exports = if source.contains("typed") {
                    vec!["default".into(), "typed".into()]
                } else {
                    vec!["default".into()]
                };
                let artifact = BundledJsArtifact::new("failed", source, exports).unwrap();
                assert!(
                    executor
                        .prepare(1, artifact, "default".into())
                        .await
                        .is_err()
                );
            }
            assert_eq!(executor.stats().evaluations, 2);
            executor
                .prepare(1, artifact.clone(), "default".into())
                .await
                .unwrap();
            assert_eq!(executor.stats().evaluations, 3);
            assert_eq!(
                executor
                    .prepare(1, artifact, "default".into())
                    .await
                    .err()
                    .unwrap()
                    .reason,
                "rotation_required"
            );
        }
        .boxed_local()
    });
}

#[test]
fn oversized_source_is_refused_before_queue_admission() {
    run(|driver| {
        async move {
            let executor = Executor::new(
                driver,
                ExecutorOptions {
                    max_source_bytes: 16,
                    ..Default::default()
                },
            )
            .unwrap();
            let artifact =
                BundledJsArtifact::new("large", "export default {};", vec!["default".into()])
                    .unwrap();
            let refusal = executor.prepare(1, artifact, "default".into());
            assert_eq!(executor.stats().queued_jobs, 0);
            assert_eq!(refusal.await.err().unwrap().reason, "source_bytes");
            assert_eq!(executor.stats().evaluations, 0);
        }
        .boxed_local()
    });
}
