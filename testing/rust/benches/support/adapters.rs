use criterion::{BatchSize, Criterion};
use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, hint::black_box, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, Placement, ValueSchema},
    rpc::*,
    *,
};
use tembra_hooks::*;

pub fn benchmarks(c: &mut Criterion) {
    hooks(c);
    rpc(c);
    typed(c);
}
fn hooks(c: &mut Criterion) {
    for handlers in [0, 1, 16] {
        let driver = ManualDriver::new();
        let host = Host::new(super::support::options(), driver.clone());
        let saved = Rc::new(RefCell::new(None));
        let slot = saved.clone();
        let facet = NativeFacet::new("benchmark.hooks", vec![], move |scope, _| {
            *slot.borrow_mut() = Some(scope.clone());
            Ok(())
        })
        .unwrap();
        driver
            .run_until(host.install(facet, LoadOptions::default()).unwrap())
            .unwrap();
        let manager = HookManager::new(
            saved.borrow_mut().take().unwrap(),
            driver.clone(),
            HookManagerOptions::default(),
        )
        .unwrap();
        let input = HookType::<u64>::serde(json!({"type":"integer"})).unwrap();
        let descriptor = HookDescriptor::new(
            "benchmark.series",
            1,
            Strategy::Series,
            input.profile().clone(),
            None,
            None,
        )
        .unwrap();
        let definition =
            RustHookDefinition::from_descriptor(descriptor, input, HookType::<()>::absent())
                .unwrap();
        for index in 0..handlers {
            let definition = definition.clone();
            let contract = Rc::new(
                Contract::local(
                    format!("benchmark.handler{index}"),
                    1,
                    json!({}),
                    BTreeMap::from([("run".into(), definition.descriptor().contribution_member())]),
                )
                .unwrap(),
            );
            host.serve(
                contract,
                Rc::new(move |_, _, declaration| {
                    definition.register(
                        declaration,
                        format!("handler{index}"),
                        index as i64,
                        "run",
                    )?;
                    let handler = hook_handler(Rc::new(|_: HookArgs<u64, ()>, _| {
                        async { Ok(()) }.boxed_local()
                    }));
                    Ok(Implementation::new()
                        .method("run", move |args: HookArgs<u64, ()>, context| {
                            handler(args, context)
                        }))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        driver.run_until(host.ready()).unwrap();
        c.bench_function(&format!("hooks/series{handlers}"), |b| {
            b.iter_batched(
                || drain(&host),
                |()| {
                    let (manager, definition) = (manager.clone(), definition.clone());
                    driver
                        .run_until(host.root(move |context| {
                            async move { manager.run_dynamic_series(&definition, 1, &context).await }
                                .boxed_local()
                        }))
                        .unwrap();
                },
                BatchSize::PerIteration,
            )
        });
        driver.run_until(host.close()).unwrap();
        report(&host, &format!("hooks/series{handlers}"));
    }
}
fn rpc(c: &mut Criterion) {
    let driver = ManualDriver::new();
    let mut server_options = super::support::options();
    server_options.name = "benchmark-server".into();
    let server = Host::new(server_options, driver.clone());
    let mut client_options = super::support::options();
    client_options.name = "benchmark-client".into();
    let client = Host::new(client_options, driver.clone());
    server
        .set_rpc_identity(
            RootOrigin::new(server.name(), "11111111111111111111111111111111").unwrap(),
        )
        .unwrap();
    client
        .set_rpc_identity(
            RootOrigin::new(client.name(), "22222222222222222222222222222222").unwrap(),
        )
        .unwrap();
    let contract = Rc::new(
        Contract::remote(
            "benchmark.rpc",
            1,
            json!({}),
            Placement::Named {
                name: server.name().into(),
            },
            BTreeMap::from([(
                "default".into(),
                BTreeMap::from([(
                    "echo".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    server
        .serve(
            contract.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("echo", |value, _| async move { Ok(value) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outbound = client
        .connect_client(left, server.name(), ConnectionOptions::default())
        .unwrap();
    let _inbound = server
        .accept_client(
            right,
            json!("benchmark-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    client.set_route(server.name(), outbound).unwrap();
    let reference = driver
        .run_until(client.get(
            contract,
            json!({}),
            GetOptions {
                state: false,
                ..Default::default()
            },
        ))
        .unwrap();
    c.bench_function("rpc/memory_roundtrip_portable_held", |b| {
        b.iter_batched(
            || {
                drain(&client);
                drain(&server);
            },
            |()| {
                black_box(
                    driver
                        .run_until(reference.call_json("echo", Some(json!(1)), None))
                        .unwrap(),
                );
            },
            BatchSize::PerIteration,
        )
    });
    reference.release();
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
    report(&client, "rpc_client");
    report(&server, "rpc_server");
}

#[tembra::contract(local, name = "benchmark.typed", version = 1)]
trait Typed {
    async fn ping(&self) -> tembra::Result<u64>;
}
struct Echo;
impl TypedProvider for Echo {
    async fn ping(&self, _context: tembra::Context) -> tembra::Result<u64> {
        Ok(1)
    }
}
fn typed(c: &mut Criterion) {
    use tembra::ContractToken;
    let driver = ManualDriver::new();
    let host = tembra::Host::new(super::support::options(), driver.clone());
    host.serve(Typed, |_, _, _| Ok(Echo)).unwrap();
    let reference = driver.run_until(host.get(Typed, TypedInput {})).unwrap();
    c.bench_function("bindings/rust_typed_held", |b| {
        b.iter_batched(
            || drain(host.core()),
            |()| {
                black_box(driver.run_until(reference.ping()).unwrap());
            },
            BatchSize::PerIteration,
        )
    });
    c.bench_function("bindings/rust_descriptor_reused", |b| {
        b.iter(|| black_box(Typed.descriptor().unwrap()))
    });
    drop(reference);
    driver.run_until(host.close()).unwrap();
    report(host.core(), "rust_typed");
}

fn drain(host: &Host) {
    let _ = host;
    #[cfg(feature = "observability-bench")]
    if let Some(observations) = host.observations() {
        let health = observations.statistics();
        assert_eq!(
            health.identifier_exhaustion, 0,
            "recording benchmark exhausted diagnostic identifiers"
        );
        assert_eq!(health.dropped_events, 0, "recording benchmark overflowed");
        black_box(observations.drain(1_048_576));
    }
}

fn report(host: &Host, case: &str) {
    drain(host);
    let _ = case;
    #[cfg(feature = "observability-bench")]
    eprintln!(
        "{}",
        serde_json::json!({"case":case,"phase":"after_cleanup","observation_health":host.observations().map(|observations|observations.statistics())})
    );
}
