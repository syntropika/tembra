use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

use futures::FutureExt;
use serde_json::json;
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

fn token(name: &str, argument: bool) -> Rc<Contract> {
    let method = Member::Method {
        args: argument.then(|| ValueSchema::Portable(json!({"type":"string"}))),
        result: Some(ValueSchema::Portable(
            json!({"type":"object","properties":{"caller":{"type":["string","null"]},"principal":{"type":"string"}},"required":["caller","principal"],"additionalProperties":false}),
        )),
    };
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            if argument {
                BTreeMap::from([("inspect".into(), method)])
            } else {
                BTreeMap::from([
                    ("inspect".into(), method.clone()),
                    ("scoped".into(), method),
                ])
            },
        )
        .unwrap(),
    )
}

#[test]
fn actual_javascript_methods_scope_roots_and_tasks_keep_owner_through_compatible_reload() {
    let driver = TokioDriver::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(driver.run_until(async {
        tokio::time::timeout(Duration::from_secs(15), verify(driver.clone()))
            .await
            .unwrap();
    }));
}

async fn verify(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("caller-js", json!("host-principal")),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let native = token("caller.native", true);
    let javascript = token("caller.javascript", false);
    let observations = Rc::new(RefCell::new(Vec::new()));
    let seen = observations.clone();
    let served = native.clone();
    let native_facet = NativeFacet::new("caller.native", vec![], move |_, setup| {
        let seen = seen.clone();
        setup.serve(
            served.clone(),
            Rc::new(move |_, _, _| {
                let seen = seen.clone();
                Ok(
                    Implementation::new().json_method("inspect", move |argument, context| {
                        seen.borrow_mut()
                            .push((argument.unwrap(), context.caller_facet().map(str::to_owned)));
                        async move {
                            Ok(Some(json!({
                                "caller": context.caller_facet(),
                                "principal": context.principal(),
                            })))
                        }
                        .boxed_local()
                    }),
                )
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    host.install(native_facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let source = format!(
        r#"
const sink = {native}, service = {javascript};
export default {{
  name: 'caller.javascript', uses: [sink],
  setup(scope, declare) {{
    declare.serve(service, (_input, self) => {{
      const target = self.get(sink, {{}});
      self.task(async task => {{ await task.root(context => target.inspect('task', context)); }});
      return {{ methods: {{
        inspect(context) {{ return target.inspect('method', context); }},
        scoped(_context) {{ return scope.root(context => target.inspect('scope', context)); }}
      }} }};
    }}, {{ lifetime: 'host' }});
  }}
}};
"#,
        native = serde_json::to_string(native.descriptor()).unwrap(),
        javascript = serde_json::to_string(javascript.descriptor()).unwrap()
    );
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("caller-facet", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet.clone(), LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(javascript, json!({}), GetOptions::default())
        .await
        .unwrap();
    for replacement in 0..2 {
        if replacement > 0 {
            host.reload(vec![facet.clone()], ReloadOptions::default())
                .await
                .unwrap();
        }
        for method in ["inspect", "scoped"] {
            assert_eq!(
                reference.call_json(method, None, None).await.unwrap(),
                Some(json!({"caller":"caller.javascript","principal":"host-principal"}))
            );
        }
    }
    host.close().await.unwrap();
    let observations = observations.borrow();
    assert!(observations.iter().any(|(source, _)| source == "task"));
    assert!(observations.iter().any(|(source, _)| source == "method"));
    assert!(observations.iter().any(|(source, _)| source == "scope"));
    assert!(
        observations
            .iter()
            .all(|(_, caller)| caller.as_deref() == Some("caller.javascript")),
        "{observations:?}"
    );
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
