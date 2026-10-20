use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;

#[path = "adapters/native.rs"]
mod native;
#[path = "adapters/refusals.rs"]
mod refusals;
#[path = "adapters/reload.rs"]
mod reload;

fn run(body: impl FnOnce(Rc<TokioDriver>) -> LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), body(driver.clone()))
                .await
                .unwrap();
        }));
}

fn portable(schema: Value) -> ValueSchema {
    ValueSchema::Portable(schema)
}

fn token(name: &str, version: u64, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, version, json!({}), members).unwrap())
}

fn pair() -> (Rc<Contract>, Rc<Contract>) {
    let provider = token(
        "adapted-counter",
        2,
        BTreeMap::from([
            (
                "add".into(),
                Member::Method {
                    args: Some(portable(json!({"type":"integer","minimum":100}))),
                    result: Some(portable(json!({"type":"integer"}))),
                },
            ),
            (
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    );
    let consumer = token(
        "adapted-counter",
        1,
        BTreeMap::from([
            (
                "increment".into(),
                Member::Method {
                    args: Some(portable(json!({"type":"integer","maximum":9}))),
                    result: Some(portable(json!({"type":"integer"}))),
                },
            ),
            (
                "total".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    );
    (provider, consumer)
}

fn descriptor(token: &Contract) -> String {
    serde_json::to_string(token.descriptor()).unwrap()
}

fn adapter(provider: &Contract, consumer: &Contract, body: &str) -> String {
    format!(
        "{{provider:{},consumer:{},adapt:current=>({body})}}",
        json!(descriptor(provider)),
        json!(descriptor(consumer)),
    )
}

async fn prepare(host: &Host, executor: &Executor, source: String) -> ExecutableFacet {
    executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("adapters", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

fn assert_closed(executor: &Executor) {
    let stats = executor.stats();
    assert_eq!(stats.callbacks, 0, "{stats:?}");
    assert_eq!(stats.capabilities, 0, "{stats:?}");
    assert_eq!(stats.contexts, 0, "{stats:?}");
    assert_eq!(stats.queued_jobs, 0, "{stats:?}");
    assert_eq!(stats.physical_jobs, 0, "{stats:?}");
}

#[test]
fn actual_javascript_consumes_legacy_aliases_with_consumer_validation_and_live_state() {
    run(|driver| portable_adapter(driver, false).boxed_local());
}

#[test]
fn newly_authored_legacy_methods_use_the_real_receiver_and_consumer_profile() {
    run(|driver| portable_adapter(driver, true).boxed_local());
}

async fn portable_adapter(driver: Rc<TokioDriver>, transform: bool) {
    let host = Host::new(HostOptions::new("adapter", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let (provider, consumer) = pair();
    let client = token(
        "adapter-client",
        1,
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: Some(portable(json!({"type":"integer"}))),
                result: Some(portable(json!({"type":"integer"}))),
            },
        )]),
    );
    let source = format!(
        r#"
const provider={provider}, consumer={consumer}, client={client};
export default {{name:'adapters',uses:[consumer],setup(scope,declare){{
    declare.serve(provider,(input,self)=>{{
        const count=self.cell('count',{{schema:{{type:'integer'}},initial:10}});
        return {{methods:{{add(ctx,amount){{
            count.change(ctx,draft=>{{draft.value+=amount;}});
            return count.value;
        }}}},state:{{count}}}};
    }},{{lifetime:'host',accepts:[{adapter}]}});
    declare.serve(client,()=>({{methods:{{async run(ctx,amount){{
        const reference=await ctx.get(consumer,{{}});
        return reference.increment(amount,ctx);
    }}}}}}));
}}}};
"#,
        provider = descriptor(&provider),
        consumer = descriptor(&consumer),
        client = descriptor(&client),
        adapter = adapter(
            &provider,
            &consumer,
            if transform {
                concat!(
                    "{methods:{increment:async(ctx,value)=>",
                    "1+await current.methods.add(ctx,value)},",
                    "state:{total:current.state.count}}"
                )
            } else {
                "{methods:{increment:current.methods.add},state:{total:current.state.count}}"
            }
        )
    );
    host.install_executable(
        prepare(&host, &executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let current = host
        .get(provider, json!({}), GetOptions::default())
        .await
        .unwrap();
    let legacy = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();
    let caller = host
        .get(client, json!({}), GetOptions::default())
        .await
        .unwrap();
    let total = legacy.readable("total").unwrap();
    assert_eq!(total.snapshot().unwrap().value(), &json!(10));
    // Legacy adaptation follows its consumer schema, not a synthetic second call through V2.
    assert_eq!(
        caller.call_json("run", Some(json!(1)), None).await.unwrap(),
        Some(json!(if transform { 12 } else { 11 }))
    );
    let invalid = legacy
        .call_json("increment", Some(json!(10)), None)
        .await
        .unwrap_err();
    assert_eq!(invalid.code, ErrorCode::Invalid);
    assert_eq!(total.snapshot().unwrap().value(), &json!(11));
    assert_eq!(
        current
            .call_json("add", Some(json!(100)), None)
            .await
            .unwrap(),
        Some(json!(111))
    );
    assert_eq!(total.snapshot().unwrap().value(), &json!(111));
    let hidden = legacy.call::<i64, i64>("add", 100, None).await.unwrap_err();
    assert_eq!(hidden.code, ErrorCode::NotInVariant);
    caller.release();
    legacy.release();
    current.release();
    host.close().await.unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_closed(&executor);
}

struct ValueCodec;
impl ScopedCodec<NativeValue> for ValueCodec {
    fn encode(&self, value: NativeValue) -> Result<NativeValue> {
        Ok(value)
    }
    fn decode(&self, value: NativeValue) -> Result<NativeValue> {
        Ok(value)
    }
}
struct Integer;
impl ScopedCodec<i64> for Integer {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        Ok(NativeValue::portable(json!(value)))
    }
    fn decode(&self, value: NativeValue) -> Result<i64> {
        value
            .into_portable()?
            .as_i64()
            .ok_or_else(|| RuntimeError::invalid("integer", "expected integer"))
    }
}

#[test]
fn active_javascript_callable_preparation_can_bind_an_adapter_in_the_same_scope() {
    run(|driver| active_scope(driver).boxed_local());
}

async fn active_scope(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("adapter-scope", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let member = || Member::Method {
        args: None,
        result: Some(portable(json!({"type":"integer"}))),
    };
    let provider = token(
        "scoped-provider",
        2,
        BTreeMap::from([("read".into(), member())]),
    );
    let consumer = token(
        "scoped-provider",
        1,
        BTreeMap::from([("oldRead".into(), member())]),
    );
    let caller = token(
        "scoped-caller",
        1,
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: Some(ValueSchema::Native("call@1".into())),
                result: Some(portable(json!({"type":"integer"}))),
            },
        )]),
    );
    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(
                Some(ValueSchema::Native("call@1".into())),
                Some(portable(json!({"type":"integer"}))),
            )
            .unwrap(),
            NativeMethodCodec::new(Rc::new(ValueCodec), Rc::new(Integer)),
        )
        .unwrap();
    let source = format!(
        r#"
const provider={provider},caller={caller};
export default {{name:'adapters',uses:[],setup(scope,declare){{
    declare.serve(provider,()=>({{methods:{{read:()=>42}}}}),{{
        lifetime:'host',accepts:[{adapter}]
    }});
    declare.serve(caller,()=>({{methods:{{run:async(ctx,action)=>await action(1)}}}}));
}}}};
"#,
        provider = descriptor(&provider),
        caller = descriptor(&caller),
        adapter = adapter(
            &provider,
            &consumer,
            "{methods:{oldRead:current.methods.read}}"
        ),
    );
    host.install_executable(
        prepare(&host, &executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let current = host
        .get(provider, json!({}), GetOptions::default())
        .await
        .unwrap();
    let caller = host
        .get(caller, json!({}), GetOptions::default())
        .await
        .unwrap();
    let entered = Rc::new(Cell::new(0));
    let recorded = entered.clone();
    let nested_host = host.clone();
    let callable = NativeCallable::deferred(Rc::new(move |_decode| {
        recorded.set(recorded.get() + 1);
        let reference = nested_host.hold(consumer.clone(), json!({}), GetOptions::default());
        let result = reference.and_then(|reference| {
            let shape = reference.shape();
            let status = reference.status();
            reference.release();
            if status != RefStatus::Live || shape.is_none() {
                return Err(RuntimeError::invalid(
                    "adapter_scope",
                    "nested binding was not live",
                ));
            }
            Ok(NativeValue::portable(json!(42)))
        });
        NativeCall::new(async move { result }.boxed_local())
    }));
    assert_eq!(
        caller
            .call::<NativeValue, i64>("run", NativeValue::callable(callable), None)
            .await
            .unwrap(),
        42
    );
    assert_eq!(entered.get(), 1);
    caller.release();
    current.release();
    host.close().await.unwrap();
    assert_closed(&executor);
}
