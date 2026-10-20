use super::*;

#[test]
fn raw_adapter_metadata_refuses_before_any_provider_factory_or_adapter_effect() {
    for case in [
        "duplicate",
        "provider",
        "same-version",
        "callable",
        "getter",
    ] {
        run(move |driver| metadata_refusal(driver, case).boxed_local());
    }
}

async fn metadata_refusal(driver: Rc<TokioDriver>, case: &str) {
    let host = Host::new(
        HostOptions::new("refused-adapter", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let (provider, consumer) = pair();
    let valid = adapter(
        &provider,
        &consumer,
        "{methods:{increment:current.methods.add},state:{total:current.state.count}}",
    );
    let entries = match case {
        "duplicate" => format!("[{valid},{valid}]"),
        "provider" => format!("[{}]", adapter(&consumer, &provider, "{}")),
        "same-version" => format!("[{}]", adapter(&provider, &provider, "{}")),
        "callable" => format!(
            "[{{provider:{},consumer:{},adapt:7}}]",
            json!(descriptor(&provider)),
            json!(descriptor(&consumer))
        ),
        "getter" => format!(
            "[{{provider:{},consumer:{},get adapt(){{globalThis.effects++;return ()=>({{}});}}}}]",
            json!(descriptor(&provider)),
            json!(descriptor(&consumer))
        ),
        _ => unreachable!(),
    };
    let source = format!(
        r#"
globalThis.effects=0;
const provider={provider};
export default{{name:'adapters',uses:[],setup(scope,declare){{
    declare.serve(provider,()=>{{globalThis.effects++;return{{}};}},{{accepts:{entries}}});
}}}};
"#,
        provider = descriptor(&provider)
    );
    let facet = prepare(&host, &executor, source).await;
    let error = host
        .install_executable(facet, LoadOptions::default())
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::Invalid, "{case}: {error:?}");
    assert_eq!(error.reason, "contract_adapter", "{case}");
    assert_eq!(
        host.hold(provider, json!({}), GetOptions::default())
            .err()
            .unwrap()
            .code,
        ErrorCode::Unavailable
    );
    // The field getter is not used to discover executable adapter callbacks.
    let check = token(
        "check-effects",
        1,
        BTreeMap::from([(
            "read".into(),
            Member::Method {
                args: None,
                result: Some(portable(json!({"type":"integer"}))),
            },
        )]),
    );
    let source = format!(
        r#"
const token={};export default{{name:'check',uses:[],setup(scope,declare){{
    declare.serve(token,()=>({{methods:{{read:()=>globalThis.effects}}}}));
}}}};
"#,
        descriptor(&check)
    );
    host.install_executable(
        prepare(&host, &executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let reference = host
        .get(check, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("read", None, None).await.unwrap(),
        Some(json!(0))
    );
    reference.release();
    host.close().await.unwrap();
    assert_closed(&executor);
}
