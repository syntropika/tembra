use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
fn token(
    name: &str,
    args: Option<ValueSchema>,
    result: Option<ValueSchema>,
    state: bool,
) -> Rc<Contract> {
    let mut members = BTreeMap::from([("run".into(), Member::Method { args, result })]);
    if state {
        members.insert(
            "count".into(),
            Member::State {
                schema: json!({ "type": "integer" }),
            },
        );
    }

    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn artifact(source: String) -> BundledJsArtifact {
    BundledJsArtifact::new("test", source, vec!["default".into()]).unwrap()
}

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
fn real_setup_cell_derive_resource_method_and_cleanup() {
    run(|driver| check_stateful_execution(driver).boxed_local());
}

struct Marker(Cell<u32>);
struct MarkerCodec;
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}

#[test]
fn same_host_native_javascript_native_javascript_preserves_identity_and_context() {
    run(|driver| check_nested_execution(driver).boxed_local());
}

async fn check_stateful_execution(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("test", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let contract = token(
        "counter",
        None,
        Some(ValueSchema::Portable(json!({ "type": "integer" }))),
        true,
    );
    let source = r#"
const token = __FIXTURE_0__;
export default {
    name: 'counter-facet', uses: [], setup(scope, d) {
        d.serve(token, (input, self) => {
            const count = self.cell('count', {
                schema: {
                    type: 'integer'
                }, initial: 2
            });
            const twice = self.derive([
                count
            ], {
                type: 'integer'
            }, ([s]) => s.value * 2);
            if (count.value !== 2 || twice.value !== 4)
                throw Error('immediate state');
            const resource = self.resource('resource', {
                open: async () => ({
                    amount: 5
                }), close: value => {
                    if (value.amount !== 5)
                        throw Error('close value');
                }
            });
            return {
                methods: {
                    run(context) {
                        count.change(context, draft => {
                            draft.value += 1;
                        });
                        return resource.value.amount + twice.value;
                    }
                }, state: {
                    count
                }
            };
        });
    }
};
"#
    .replace(
        "__FIXTURE_0__",
        &(serde_json::to_string(contract.descriptor()).unwrap()),
    );
    let facet = executor
        .prepare(host.id(), artifact(source), "default".into())
        .await
        .unwrap();
    let startup = host
        .install_executable(facet, LoadOptions::default())
        .unwrap();
    let reference = host
        .get(contract, json!({}), GetOptions::default())
        .await
        .unwrap();
    startup.await.unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(11))
    );
    assert_eq!(
        reference
            .readable("count")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(3)
    );
    assert_eq!(executor.stats().contexts, 0);
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}

async fn check_nested_execution(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("test", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)),
        )
        .unwrap();
    let contract = |name| {
        token(
            name,
            Some(ValueSchema::Native("marker@1".into())),
            Some(ValueSchema::Native("marker@1".into())),
            false,
        )
    };
    let a = contract("a");
    let b = contract("b");
    let c = contract("c");
    let d = contract("d");
    let source = r#"
const b = __FIXTURE_0__, c = __FIXTURE_1__, d = __FIXTURE_2__;
export default {
    name: 'nested', uses: [
        c
    ], setup(scope, declare) {
        declare.serve(b, (input, self) => {
            const dependency = self.get(c, {});
            return {
                methods: {
                    async run(context, arg) {
                        context.check();
                        return await dependency.run(arg, context);
                    }
                }, state: {}
            };
        });
        declare.serve(d, () => ({
            methods: {
                async run(context, arg) {
                    context.check();
                    return arg;
                }
            }, state: {}
        }));
    }
};
"#
    .replace(
        "__FIXTURE_0__",
        &(serde_json::to_string(b.descriptor()).unwrap()),
    )
    .replace(
        "__FIXTURE_1__",
        &(serde_json::to_string(c.descriptor()).unwrap()),
    )
    .replace(
        "__FIXTURE_2__",
        &(serde_json::to_string(d.descriptor()).unwrap()),
    );
    let facet = executor
        .prepare(host.id(), artifact(source), "default".into())
        .await
        .unwrap();
    host.install_executable(
        facet,
        LoadOptions {
            await_host_objects: false,
        },
    )
    .unwrap()
    .await
    .unwrap();
    let root_trace = Rc::new(Cell::new(None));
    let a_trace = root_trace.clone();
    let b_dep = b.clone();
    host.serve(
        a.clone(),
        Rc::new(move |_, _, declare| {
            let dep = declare.get(b_dep.clone(), json!({}), GetOptions::default())?;
            let a_trace = a_trace.clone();
            Ok(
                Implementation::new().method("run", move |arg: Rc<Marker>, context| {
                    let dep = dep.clone();
                    a_trace.set(Some(context.root_id()));
                    async move {
                        arg.0.set(arg.0.get() + 1);
                        dep.call::<Rc<Marker>, Rc<Marker>>("run", arg, Some(&context))
                            .await
                    }
                    .boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let c_trace = root_trace.clone();
    let d_dep = d.clone();
    host.serve(
        c,
        Rc::new(move |_, _, declare| {
            let dep = declare.get(d_dep.clone(), json!({}), GetOptions::default())?;
            let c_trace = c_trace.clone();
            Ok(
                Implementation::new().method("run", move |arg: Rc<Marker>, context| {
                    let dep = dep.clone();
                    assert_eq!(c_trace.get(), Some(context.root_id()));
                    async move {
                        assert_eq!(context.code_version(), 1);
                        assert_eq!(context.principal(), &json!("host"));
                        arg.0.set(arg.0.get() + 1);
                        dep.call::<Rc<Marker>, Rc<Marker>>("run", arg, Some(&context))
                            .await
                    }
                    .boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host.get(a, json!({}), GetOptions::default()).await.unwrap();
    let marker = Rc::new(Marker(Cell::new(0)));
    let result = reference
        .call::<Rc<Marker>, Rc<Marker>>("run", marker.clone(), None)
        .await
        .unwrap();
    assert!(Rc::ptr_eq(&marker, &result));
    assert_eq!(marker.0.get(), 2);
    assert!(root_trace.get().is_some());
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}
