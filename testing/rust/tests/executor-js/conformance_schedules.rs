//! Fixed-seed schedules through actual V8, checking effects and live ownership mid-Host.
use futures::FutureExt;
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;

const SEED: u64 = 0x7465_6d62_7261_0009;

struct Marker;
#[derive(Default)]
struct Codecs {
    encoded: Cell<usize>,
    decoded: Cell<usize>,
}
struct MarkerCodec(Rc<Codecs>);
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        self.0.encoded.set(self.0.encoded.get() + 1);
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        self.0.decoded.set(self.0.decoded.get() + 1);
        value.downcast()
    }
}

fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    *seed
}

fn tree(seed: &mut u64, depth: u8) -> Value {
    let variant = (next(seed) >> 32) % if depth == 0 { 5 } else { 8 };
    match variant {
        0 => Value::Null,
        1 => json!(false),
        2 => json!((next(seed) % 4096) as i64 - 2048),
        3 => json!(""),
        4 => json!("é🙂"),
        5 => json!([]),
        6 => json!([tree(seed, depth - 1), tree(seed, depth - 1)]),
        _ => json!({"value":tree(seed, depth - 1),"é":tree(seed, depth - 1)}),
    }
}

fn portable(args: Option<ValueSchema>) -> Member {
    Member::Method {
        args,
        result: Some(ValueSchema::Portable(json!({}))),
    }
}

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn quiescent(executor: &Executor) -> ExecutorStats {
    let stats = executor.stats();
    assert_eq!(
        (stats.contexts, stats.queued_jobs, stats.physical_jobs),
        (0, 0, 0)
    );
    stats
}

#[test]
fn generated_values_and_repeated_release_refusal_schedules_preserve_effects_and_live_bounds() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(20), scenario(driver.clone()))
                .await
                .unwrap();
        }));
}

async fn scenario(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("schedule", json!("host")), driver.clone());
    let executor = Executor::new(
        driver,
        ExecutorOptions {
            max_evaluations: 1,
            max_capabilities: 64,
            max_callbacks: 64,
            ..Default::default()
        },
    )
    .unwrap();
    let codecs = Rc::new(Codecs::default());
    executor
        .register_native_method(
            NativeMethodSignature::new(
                Some("schedule.marker@1".into()),
                Some("schedule.marker@1".into()),
            )
            .unwrap(),
            NativeMethodCodec::new(
                Rc::new(MarkerCodec(codecs.clone())),
                Rc::new(MarkerCodec(codecs.clone())),
            ),
        )
        .unwrap();
    let any = Some(ValueSchema::Portable(json!({})));
    let native_member = Member::Method {
        args: Some(ValueSchema::Native("schedule.marker@1".into())),
        result: Some(ValueSchema::Native("schedule.marker@1".into())),
    };
    let dependency = token(
        "schedule.native",
        BTreeMap::from([
            ("round".into(), portable(any.clone())),
            ("identity".into(), native_member.clone()),
        ]),
    );
    let portable_effects = Rc::new(Cell::new(0));
    let native_effects = Rc::new(Cell::new(0));
    let (portable_count, native_count) = (portable_effects.clone(), native_effects.clone());
    host.serve(
        dependency.clone(),
        Rc::new(move |_, _, _| {
            let (portable_count, native_count) = (portable_count.clone(), native_count.clone());
            Ok(Implementation::new()
                .json_method("round", move |value, _| {
                    portable_count.set(portable_count.get() + 1);
                    async move { Ok(value) }.boxed_local()
                })
                .method("identity", move |value: Rc<Marker>, _| {
                    native_count.set(native_count.get() + 1);
                    async move { Ok(value) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let consumer = token(
        "schedule.javascript",
        BTreeMap::from([
            ("round".into(), portable(any.clone())),
            ("identity".into(), native_member),
            ("reject".into(), portable(any)),
            ("forged".into(), portable(None)),
            ("getters".into(), portable(None)),
        ]),
    );
    let source = format!(
        r#"
const native={native}, token={consumer};
let getters=0;
export default {{name:'schedule',uses:[native],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        const dependency=self.get(native,{{}});
        const count=self.cell('count',{{schema:{{type:'integer'}},initial:0}});
        return {{methods:{{
            round:(ctx,value)=>dependency.round(value,ctx),
            identity:(ctx,value)=>dependency.identity(value,ctx),
            async reject(ctx,mode){{
                let value;
                if(mode===0) value={{get hidden(){{getters++;return 1;}}}};
                if(mode===1) value=Object.defineProperty({{}},'hidden',{{value:1}});
                if(mode===2) value=Array(2);
                if(mode===3) value={{[Symbol('hidden')]:1}};
                const stop=count.subscribe(()=>{{}});
                stop();stop();
                try{{await dependency.round(value,ctx);return ['accepted'];}}
                catch(error){{return [error.code,error.reason,getters];}}
            }},
            async forged(ctx){{
                try{{await dependency.identity({{}},ctx);return ['accepted'];}}
                catch(error){{return [error.code,error.reason];}}
            }},
            getters:()=>getters
        }},state:{{}}}};
    }});
}}}};
"#,
        native = serde_json::to_string(dependency.descriptor()).unwrap(),
        consumer = serde_json::to_string(consumer.descriptor()).unwrap()
    );
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("schedule", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let keeper = host
        .get(consumer.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    let native = host
        .get(dependency, json!({}), GetOptions::default())
        .await
        .unwrap();
    let marker = Rc::new(Marker);
    let actual: Rc<Marker> = native.call("identity", marker.clone(), None).await.unwrap();
    assert!(Rc::ptr_eq(&marker, &actual));
    assert_eq!((codecs.encoded.get(), codecs.decoded.get()), (0, 0));
    let before = portable_effects.get();
    let absent = keeper.call_json("round", None, None).await.unwrap_err();
    assert_eq!(absent.code, ErrorCode::Invalid);
    assert_eq!(portable_effects.get(), before);
    assert_eq!(
        keeper
            .call_json("round", Some(Value::Null), None)
            .await
            .unwrap(),
        Some(Value::Null)
    );
    // Warm all issued reusable handles before observing the live plateau.
    let _: Rc<Marker> = keeper.call("identity", marker.clone(), None).await.unwrap();
    assert_eq!(
        keeper
            .call_json("reject", Some(json!(0)), None)
            .await
            .unwrap()
            .unwrap()[0],
        "invalid"
    );
    tokio::task::yield_now().await;
    let baseline = quiescent(&executor);
    let mut seed = SEED;
    let mut released_wrappers = Vec::new();
    for step in 0..96 {
        let input = tree(&mut seed, 3);
        let before = portable_effects.get();
        let result = keeper
            .call_json("round", Some(input.clone()), None)
            .await
            .unwrap();
        assert_eq!(result, Some(input), "seed={SEED:x} step={step}");
        assert_eq!(portable_effects.get(), before + 1);
        let before = native_effects.get();
        let returned: Rc<Marker> = keeper.call("identity", marker.clone(), None).await.unwrap();
        assert!(Rc::ptr_eq(&marker, &returned), "seed={SEED:x} step={step}");
        assert_eq!(native_effects.get(), before + 1);
        let before = portable_effects.get();
        let refused = keeper
            .call_json("reject", Some(json!(next(&mut seed) % 4)), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            refused[0], "invalid",
            "seed={SEED:x} step={step}: {refused}"
        );
        assert!(refused[1].as_str().is_some_and(|reason| !reason.is_empty()));
        assert_eq!(refused[2], 0);
        assert_eq!(portable_effects.get(), before);
        let before = native_effects.get();
        let forged = keeper
            .call_json("forged", None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(forged, json!(["invalid", "native_type"]));
        assert_eq!(native_effects.get(), before);
        let alias = host
            .hold(consumer.clone(), json!({}), GetOptions::default())
            .unwrap();
        alias.ready().await.unwrap();
        alias.release();
        alias.release();
        let closed = alias
            .call_json("round", Some(json!(null)), None)
            .await
            .unwrap_err();
        assert_eq!(closed.code, ErrorCode::Released);
        released_wrappers.push(alias);
        tokio::task::yield_now().await;
        let current = quiescent(&executor);
        assert_eq!(
            current.callbacks, baseline.callbacks,
            "seed={SEED:x} step={step}"
        );
        assert_eq!(
            current.capabilities, baseline.capabilities,
            "seed={SEED:x} step={step}"
        );
        assert_eq!(current.evaluations, 1);
        assert_eq!(current.source_bytes, baseline.source_bytes);
    }
    assert_eq!(
        keeper.call_json("getters", None, None).await.unwrap(),
        Some(json!(0))
    );
    assert_eq!(released_wrappers.len(), 96);
    assert!(codecs.encoded.get() > 0 && codecs.decoded.get() > 0);
    keeper.release();
    native.release();
    host.close().await.unwrap();
    tokio::task::yield_now().await;
    let closed = quiescent(&executor);
    assert_eq!((closed.callbacks, closed.capabilities), (0, 0));
    assert_eq!(closed.evaluations, 1);
    assert!(
        !released_wrappers.is_empty(),
        "released wrappers remain retained at cleanup"
    );
}
