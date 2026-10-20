use std::{collections::BTreeMap, error::Error, rc::Rc};

use futures::FutureExt;
use serde_json::json;
use tembra_core::{
    ConflictPolicy, GetOptions, Host, HostOptions, Implementation, Lifetime, LoadOptions,
    ManualDriver, NativeFacet, Registry, RuntimeError, ServeOptions,
    contracts::{Contract, Member, ValueSchema},
};

fn service(name: &str) -> Result<Rc<Contract>, Box<dyn Error>> {
    Ok(Rc::new(Contract::local(
        name,
        1,
        json!({}),
        BTreeMap::from([(
            "apply".into(),
            Member::Method {
                args: Some(ValueSchema::Native("example.amount@1".into())),
                result: Some(ValueSchema::Native("example.amount@1".into())),
            },
        )]),
    )?))
}

fn main() -> Result<(), Box<dyn Error>> {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("facet-example", json!("runtime")),
        driver.clone(),
    );
    let operations = Registry::new("example.operations", ConflictPolicy::Error)?;
    let manager = service("example.manager")?;
    let worker = service("example.worker")?;
    let manager_token = manager.clone();
    let provider_registry = operations.clone();
    let consumer = NativeFacet::new(
        "example.consumer",
        vec![worker.clone()],
        move |scope, setup| {
            let view = scope.registry(&operations)?;
            setup.serve(
                manager.clone(),
                Rc::new(move |_, _, _| {
                    let view = view.clone();
                    Ok(
                        Implementation::new().method("apply", move |input: i32, ctx| {
                            let view = view.clone();
                            async move {
                                let table = view.table(Some(&ctx))?;
                                let operation = table
                                    .get("double")
                                    .and_then(|entry| entry.operation())
                                    .cloned()
                                    .ok_or_else(|| {
                                        RuntimeError::unavailable("operation", "double is absent")
                                    })?;
                                operation.call::<i32, i32>(&ctx, input).await
                            }
                            .boxed_local()
                        }),
                    )
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..ServeOptions::default()
                },
            )
        },
    )?;
    let provider = NativeFacet::new("example.provider", vec![], move |_, setup| {
        let registry = provider_registry.clone();
        setup.serve(
            worker.clone(),
            Rc::new(move |_, _, declaration| {
                declaration.register_method(
                    &registry,
                    "double",
                    json!({"label": "Double"}),
                    "apply",
                )?;
                Ok(Implementation::new().method("apply", |input: i32, _| {
                    async move { Ok(input * 2) }.boxed_local()
                }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..ServeOptions::default()
            },
        )
    })?;
    let startup = host.load(vec![consumer, provider], LoadOptions::default())?;
    driver.run_until(startup)?;
    let reference = driver.run_until(host.get(manager_token, json!({}), GetOptions::default()))?;
    let result = driver.run_until(reference.call::<i32, i32>("apply", 21, None))?;
    assert_eq!(result, 42);
    println!("facets: manager(21) = {result}");
    reference.release();
    driver.run_until(host.close())?;
    Ok(())
}
