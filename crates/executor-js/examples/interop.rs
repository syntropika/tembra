//! One Host: Rust A → JavaScript B → Rust C → JavaScript D.
use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
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

fn contract(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("marker@1".into())),
                    result: Some(ValueSchema::Native("marker@1".into())),
                },
            )]),
        )
        .unwrap(),
    )
}

fn main() -> Result<()> {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(interop(driver.clone())))
}

async fn interop(driver: Rc<TokioDriver>) -> Result<()> {
    let host = Host::new(
        HostOptions::new("interop", json!("application")),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default())?;
    executor.register_native_method(
        NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into()))?,
        NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)),
    )?;
    let (a, b, c, d) = (contract("a"), contract("b"), contract("c"), contract("d"));
    let source = r#"
const b = __FIXTURE_0__, c = __FIXTURE_1__, d = __FIXTURE_2__;
export default {
    name: 'javascript-legs', uses: [
        c
    ], setup(scope, declare) {
        declare.serve(b, (input, self) => {
            const dependency = self.get(c, {});
            return {
                methods: {
                    run: async (ctx, value) => dependency.run(value, ctx)
                }, state: {}
            };
        });
        declare.serve(d, () => ({
            methods: {
                run: (ctx, value) => {
                    ctx.check();
                    return value;
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
    let artifact = BundledJsArtifact::new("javascript-legs", source, vec!["default".into()])?;
    let facet = executor
        .prepare(host.id(), artifact, "default".into())
        .await?;
    host.install_executable(
        facet,
        LoadOptions {
            await_host_objects: false,
        },
    )?
    .await?;
    for (provided, dependency) in [(a.clone(), b), (c, d)] {
        host.serve(
            provided,
            Rc::new(move |_, _, declare| {
                let reference =
                    declare.get(dependency.clone(), json!({}), GetOptions::default())?;
                Ok(
                    Implementation::new().method("run", move |value: Rc<Marker>, ctx| {
                        value.0.set(value.0.get() + 1);
                        let reference = reference.clone();
                        async move {
                            reference
                                .call::<Rc<Marker>, Rc<Marker>>("run", value, Some(&ctx))
                                .await
                        }
                        .boxed_local()
                    }),
                )
            }),
            ServeOptions::default(),
        )?;
    }

    let reference = host.get(a, json!({}), GetOptions::default()).await?;
    let marker = Rc::new(Marker(Cell::new(0)));
    let result = reference
        .call::<Rc<Marker>, Rc<Marker>>("run", marker.clone(), None)
        .await?;
    assert!(Rc::ptr_eq(&marker, &result));
    println!("same native Rc: true; Rust visits: {}", result.0.get());
    reference.release();
    host.close().await?;
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().callbacks, 0);
    Ok(())
}
