use futures::{FutureExt, future::LocalBoxFuture};

use serde_json::json;

use std::{collections::BTreeMap, rc::Rc, time::Duration};

use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

use tembra_executor_js::*;

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

async fn install(host: &Host, executor: &Executor, name: &str, source: String) {
    let facet = executor
        .prepare_for_host(
            host,
            BundledJsArtifact::new(name, source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();

    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
}

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

#[test]
fn declarations_reject_other_host_state_resource_dependencies_and_opaque_exports() {
    run(|driver| {
        scenario_declarations_reject_other_host_state_resource_dependencies_and_opaque_exports(
            driver,
        )
        .boxed_local()
    });
}

async fn scenario_declarations_reject_other_host_state_resource_dependencies_and_opaque_exports(
    driver: Rc<TokioDriver>,
) {
    let a = Host::new(HostOptions::new("a", json!("a")), driver.clone());
    let b = Host::new(HostOptions::new("b", json!("b")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

    let state = Member::State {
        schema: json!({
        "type":"integer"}
        ),
    };
    let a_token = token("a", BTreeMap::from([("count".into(), state.clone())]));

    let dependency = token("dependency", BTreeMap::new());
    a.serve(
        dependency.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default(),
    )
    .unwrap();

    let source = format!(
        r#"
const token={},dependency={};
export default {{
    name:'a',uses:[],setup(scope,d){{
        d.serve(token,(input,self)=>{{
            const count=self.cell('count',{{
                schema:{{
                    type:'integer'
                }}
                ,initial:1
            }}
            );
            const resource=self.resource('resource',{{
                open:()=>1
            }}
            );
            globalThis.fromA={{
                count,resource,dependency:self.get(dependency,{{
                }}
                )
            }}
            ;
            return {{
                methods:{{
                }}
                ,state:{{
                    count
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(a_token.descriptor()).unwrap(),
        serde_json::to_string(dependency.descriptor()).unwrap()
    );
    install(&a, &executor, "a", source).await;
    let held = a
        .get(a_token, json!({}), GetOptions::default())
        .await
        .unwrap();

    let check = token(
        "check",
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: None,
                result: Some(ValueSchema::Portable(json!({
                "type":"integer"}
                ))),
            },
        )]),
    );
    let foreign_state = token("foreign-state", BTreeMap::from([("count".into(), state)]));
    let foreign_opaque = token(
        "foreign-opaque",
        BTreeMap::from([(
            "opaque".into(),
            Member::Opaque {
                id: "marker@1".into(),
            },
        )]),
    );

    let source = format!(
        r#"
const token={},state={},opaque={};
export default {{
    name:'b',uses:[],setup(scope,d){{
        d.serve(token,(input,self)=>{{
            let refused=0;
            for(const action of [()=>self.derive([fromA.count],{{
                type:'integer'
            }}
            ,([s])=>s.value),()=>self.resource('ref',{{
                deps:[fromA.dependency],open:()=>1
            }}
            ),()=>self.resource('res',{{
                deps:[fromA.resource],open:()=>1
            }}
            )]){{
                try{{
                    action();
                    throw Error('cross Host accepted');
                }}
                catch(e){{
                    if(e.reason!=='foreign_host')throw e;
                    refused++;
                }}
            }}
            return {{
                methods:{{
                    run:()=>refused
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
        d.serve(state,()=>({{
            methods:{{
            }}
            ,state:{{
                count:fromA.count
            }}
        }}
        ));
        d.serve(opaque,()=>({{
            methods:{{
            }}
            ,state:{{
            }}
            ,opaque:{{
                opaque:fromA.resource
            }}
        }}
        ));
    }}
}}
;
"#,
        serde_json::to_string(check.descriptor()).unwrap(),
        serde_json::to_string(foreign_state.descriptor()).unwrap(),
        serde_json::to_string(foreign_opaque.descriptor()).unwrap()
    );
    install(&b, &executor, "b", source).await;

    let reference = b
        .get(check, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(3))
    );
    reference.release();

    for token in [foreign_state, foreign_opaque] {
        let error = b
            .get(token, json!({}), GetOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(error.reason, "foreign_host");
    }

    b.close().await.unwrap();
    assert!(held.readable("count").unwrap().snapshot().is_ok());
    held.release();
    a.close().await.unwrap();
    assert_eq!(executor.stats().capabilities, 0);
    assert_eq!(executor.stats().callbacks, 0);
}
