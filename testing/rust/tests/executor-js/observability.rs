use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    observability::Operation,
    *,
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

#[test]
fn provider_diagnostic_getters_join_the_actual_core_entry_without_new_ownership() {
    for enabled in [false, true] {
        let driver = TokioDriver::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(driver.run_until(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut options = HostOptions::new("diagnostic-getters", json!(null));
                    if enabled {
                        options = options.with_observability(ObservationOptions::new(1024, 1).unwrap());
                    }
                    let host = Host::new(options, driver.clone());
                    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
                    let contract = Rc::new(Contract::local("diagnostic-getters", 1, json!({}), BTreeMap::from([
                        ("run".into(), Member::Method {
                            args: None,
                            result: Some(ValueSchema::Portable(json!({
                                "type":"object", "properties":{"host":{"type":"string"},"id":{"type":["string","null"]}},
                                "required":["host","id"],"additionalProperties":false
                            }))),
                        })
                    ])).unwrap());
                    let source = r#"
const token=__TOKEN__;
export default {name:'diagnostic-getters',uses:[],setup(_s,d){
  d.serve(token,()=>({state:{},methods:{run(ctx){
    return {host:ctx.observationHostId,id:ctx.observationId};
  }}}));
}};
"#.replace("__TOKEN__", &serde_json::to_string(contract.descriptor()).unwrap());
                    let artifact = BundledJsArtifact::new("diagnostic-getters", source, vec!["default".into()]).unwrap();
                    let facet = executor.prepare_for_host(&host, artifact, "default".into()).await.unwrap();
                    let startup = host.install_executable(facet, LoadOptions::default()).unwrap();
                    let reference = host.get(contract, json!({}), GetOptions::default()).await.unwrap();
                    startup.await.unwrap();
                    let identity = reference.call_json("run", None, None).await.unwrap().unwrap();
                    assert_eq!(identity["host"], host.id().to_string());
                    if let Some(observations) = host.observations() {
                        let events = observations.drain(1024);
                        assert!(events.iter().any(|event| event.operation == Operation::PhysicalInvocation
                            && Some(event.id.to_string()) == identity["id"].as_str().map(str::to_owned)
                            && event.host == host.id()));
                        assert_eq!(observations.statistics().dropped_events, 0);
                        assert_eq!(observations.statistics().identifier_exhaustion, 0);
                    } else {
                        assert!(identity["id"].is_null());
                    }
                    reference.release();
                    host.close().await.unwrap();
                    assert_eq!(executor.stats().physical_jobs, 0);
                    assert_eq!(executor.stats().contexts, 0);
                    assert_eq!(executor.stats().callbacks, 0);
                    if let Some(observations) = host.observations() {
                        assert_eq!(observations.statistics().dropped_events, 0);
                    }
                }).await.unwrap();
            }));
    }
}
