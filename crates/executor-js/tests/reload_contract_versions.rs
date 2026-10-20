//! Real JS contract bumps retoken host pins; surviving old consumers still need adapters.
use futures::FutureExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;

fn token(version: u64) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "versioned.javascript",
            version,
            json!({}),
            BTreeMap::from([
                (
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(if version == 1 {
                            json!({"type":"integer"})
                        } else {
                            json!({"type":"string"})
                        })),
                    },
                ),
                (
                    "metrics".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                    },
                ),
            ]),
        )
        .unwrap(),
    )
}

async fn prepare(
    host: &Host,
    executor: &Executor,
    token: &Contract,
    version: u64,
) -> ExecutableFacet {
    let result = if version == 1 { "1" } else { "'two'" };
    let source = format!(
        r#"
const token={token};
export default {{name:'versioned.javascript',uses:[],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        const resource=self.resource('kept',{{
            version:1,
            open(){{
                globalThis.versionOpens=(globalThis.versionOpens??0)+1;
                return {{origin:{version}}};
            }},
            close(){{globalThis.versionCloses=(globalThis.versionCloses??0)+1;}}
        }});
        return {{methods:{{
            read:()=>{result},
            metrics:()=>[globalThis.versionOpens,globalThis.versionCloses??0,resource.value.origin]
        }},state:{{}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
        token = serde_json::to_string(token.descriptor()).unwrap()
    );
    executor
        .prepare(
            host.id(),
            BundledJsArtifact::new(format!("version-{version}"), source, vec!["default".into()])
                .unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

fn run(retain_old_consumer: bool) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                scenario(driver.clone(), retain_old_consumer),
            )
            .await
            .unwrap();
        }));
}

#[test]
fn real_js_contract_version_bump_with_only_host_pin_retokens_and_keeps_object_resource() {
    run(false);
}

#[test]
fn real_js_version_bump_refuses_surviving_old_consumer_without_adapter_then_retries_after_release()
{
    run(true);
}

async fn scenario(driver: Rc<TokioDriver>, retain_old_consumer: bool) {
    let host = Host::new(HostOptions::new("versions", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let old = token(1);
    let new = token(2);
    let first = prepare(&host, &executor, &old, 1).await;
    host.install_executable(first, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(old.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    let object_id = reference.object_id();
    assert_eq!(
        reference.call_json("read", None, None).await.unwrap(),
        Some(json!(1))
    );
    assert_eq!(
        reference.call_json("metrics", None, None).await.unwrap(),
        Some(json!([1, 0, 1]))
    );
    let second = prepare(&host, &executor, &new, 2).await;
    if retain_old_consumer {
        let error = host
            .reload(vec![second.clone()], ReloadOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ContractMismatch);
        assert_eq!(
            reference.call_json("read", None, None).await.unwrap(),
            Some(json!(1))
        );
        assert_eq!(
            reference.call_json("metrics", None, None).await.unwrap(),
            Some(json!([1, 0, 1]))
        );
        assert_eq!(reference.shape().unwrap().contract, old.fingerprint());
        let epoch = host
            .root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
            .await
            .unwrap();
        assert_eq!(epoch, 1);
    }
    // The released wrapper stays alive but is not a surviving consumer. Only the actual host
    // lifecycle pin must retoken, without imposing a legacy adapter on the new JS declaration.
    reference.release();
    let report = host
        .reload(vec![second], ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(report.current_version(), 2);
    let current = host
        .get(new.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(current.object_id(), object_id);
    assert_eq!(current.contract().fingerprint(), new.fingerprint());
    assert_eq!(current.shape().unwrap().contract, new.fingerprint());
    assert_eq!(
        current.call_json("read", None, None).await.unwrap(),
        Some(json!("two"))
    );
    assert_eq!(
        current.call_json("metrics", None, None).await.unwrap(),
        Some(json!([1, 0, 1]))
    );
    let late_old = host
        .hold(old, json!({}), GetOptions::default())
        .err()
        .unwrap();
    assert_eq!(late_old.code, ErrorCode::ContractMismatch);
    assert_eq!(
        current.call_json("read", None, None).await.unwrap(),
        Some(Value::String("two".into()))
    );
    current.release();
    host.close().await.unwrap();
    let stats = executor.stats();
    assert_eq!(
        (
            stats.callbacks,
            stats.capabilities,
            stats.contexts,
            stats.queued_jobs,
            stats.physical_jobs
        ),
        (0, 0, 0, 0, 0)
    );
}
