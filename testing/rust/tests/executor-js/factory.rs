use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
#[test]
fn synchronous_recorder_immediately_validates_and_refuses_promise_factory_or_factory_mutation() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                check_factory_recorder(driver.clone()),
            )
            .await
            .unwrap();
        }));
}

async fn check_factory_recorder(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("factory", json!(null)), driver.clone());
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let token = Rc::new(
        Contract::local(
            "factory",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({ "type": "integer" }))),
                },
            )]),
        )
        .unwrap(),
    );
    let source = format!(
        r#"
const token = {};
let task;
export default {{
    name: 'factory', uses: [], setup(scope, d) {{
        d.serve(token, (input, self) => {{
            const count = self.cell('count', {{
                schema: {{
                    type: 'integer'
                }}, initial: 1
            }});
            if (count.value !== 1)
                throw Error('read after declaration');
            let caught = false;
            try {{
                self.cell('invalid', {{
                    schema: {{
                        type: 'integer'
                    }}, initial: 'wrong'
                }});
            }}
            catch (e) {{
                caught = true;
            }}
            if (!caught)
                throw Error('validation was deferred');
            try {{
                count.replace(globalThis.active, 2);
                throw Error('write accepted');
            }}
            catch (e) {{
                if (e.reason !== 'read_only')
                    throw e;
            }}
            if (count.value !== 1)
                throw Error('factory changed cell');
            const value = self.resource('value', {{
                open: () => 7
            }});
            try {{
                value.value;
                throw Error('resource read accepted');
            }}
            catch (e) {{
                if (e.reason !== 'factory')
                    throw e;
            }}
            return {{
                methods: {{
                    run: () => {{
                        globalThis.factoryDone();
                        return count.value;
                    }}
                }}, state: {{}}
            }};
        }});
        task = scope.root(async (ctx) => {{
            globalThis.active = ctx;
            await ctx.get(token, {{}});
            await new Promise(resolve => globalThis.factoryDone = resolve);
        }});
    }}
}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    let artifact = BundledJsArtifact::new("factory", source, vec!["default".into()]).unwrap();
    let facet = executor
        .prepare_for_host(&host, artifact, "default".into())
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    for _ in 0..20 {
        if executor.stats().contexts > 0 {
            break;
        }

        tokio::task::yield_now().await;
    }

    assert!(
        executor.stats().contexts > 0,
        "{:?}",
        executor.take_errors()
    );
    let reference = host
        .get(token, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(1))
    );
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    let host = Host::new(
        HostOptions::new("promise-factory", json!(null)),
        driver.clone(),
    );
    let token = Rc::new(Contract::local("promise", 1, json!({}), BTreeMap::new()).unwrap());
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'promise', uses: [], setup(s, d) {{
        d.serve(token, () => Promise.resolve({{
            methods: {{}}, state: {{}}
        }}));
    }}
}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    let facet = executor
        .prepare_for_host(
            &host,
            BundledJsArtifact::new("promise", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        host.get(token, json!({}), GetOptions::default())
            .await
            .err()
            .unwrap()
            .reason,
        "async_declaration"
    );
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}
