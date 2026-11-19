//! Real macro-generated consumers call a fixed-WIT native provider through portable ownership.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, Provider};
use tembra_orb_native_fixture::codec::{exporter as provider, importer as consumer};

#[test]
fn a_native_consumer_calls_sync_functions_and_owns_decoder_resources() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        provider::orb_named("codec", 1).unwrap(),
        consumer::orb_named(
            "pipeline",
            1,
            consumer::Providers {
                codec: Provider::new("codec", 1),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let pipeline = Rc::new(
        host.get(
            "pipeline::example:codec-consumer/pipeline@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    pool.run_until(pipeline.ready()).unwrap();
    let claims = host.kernel().diagnostics().references;
    let call = pipeline.clone();
    let result = pool
        .run_until(host.root("host", move |context| {
            async move {
                call.call_portable::<_, (u64, u32)>(&context, "run", (10_u64,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(result, (51, 0));
    assert_eq!(host.kernel().diagnostics().references, claims);
    let codec = Rc::new(
        host.get(
            "codec::example:protocol/codec@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    let call = codec.clone();
    let dropped = pool
        .run_until(host.root("host", move |context| {
            async move {
                call.call_portable::<_, u32>(&context, "dropped", Vec::<u32>::new())
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(dropped, 5);
    codec.release().unwrap();
    pipeline.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A retained imported owner cannot silently redirect to a replacement provider revision.
#[test]
fn imported_resources_retire_on_reload_and_calls_after_close_are_refused() {
    use serde_json::{Value, json};
    use tembra::Code;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        provider::orb_named("codec", 1).unwrap(),
        consumer::orb_named(
            "pipeline",
            1,
            consumer::Providers {
                codec: Provider::new("codec", 1),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let pipeline = Rc::new(
        host.get(
            "pipeline::example:codec-consumer/pipeline@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    let call = |pool: &mut LocalPool, method: &'static str, arguments: Value| {
        let pipeline = pipeline.clone();
        pool.run_until(host.root("host", move |context| {
            async move {
                pipeline
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
            }
            .boxed_local()
        }))
    };
    call(&mut pool, "keep", json!([17])).unwrap();
    assert_eq!(call(&mut pool, "read", json!([])).unwrap(), json!(17));
    pool.run_until(host.reload(vec![provider::orb_named("codec", 1).unwrap()], 5_000))
        .unwrap();
    let error = call(&mut pool, "read", json!([])).unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Gone, "resource")
    );
    call(&mut pool, "discard", json!([])).unwrap();
    call(&mut pool, "keep", json!([23])).unwrap();
    assert_eq!(call(&mut pool, "read", json!([])).unwrap(), json!(23));
    call(&mut pool, "discard", json!([])).unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(call(&mut pool, "read", json!([])).is_err());
    pipeline.release().unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}

/// Provider acquisition failure never creates an imported resource owner or leaves root claims.
#[test]
fn failed_provider_acquisition_leaves_no_import_claims_after_close() {
    use serde_json::Value;
    use tembra::{Code, Error, Orb, Service, portable::Payload};
    let declaration = provider::orb_named("codec", 1).unwrap();
    let metadata = &declaration.declaration().services[0];
    let mut service = Service::new("codec::example:protocol/codec@1.0.0", |_| {
        Err::<(), _>(Error::new(
            Code::Forbidden,
            "fixture",
            "provider acquisition refused",
        ))
    })
    .signature(metadata.signature.clone());
    for method in &metadata.methods {
        service = service.payload_method(method.clone(), |_, _, _: Payload| async {
            Ok(Payload::data(Value::Null))
        });
    }
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = host
        .install(vec![
            Orb::new("codec", 1).service(service),
            consumer::orb_named(
                "pipeline",
                1,
                consumer::Providers {
                    codec: Provider::new("codec", 1),
                },
            )
            .unwrap(),
        ])
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Forbidden, "fixture")
    );
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
    assert_eq!(host.kernel().diagnostics().calls, 0);
}

#[test]
fn an_imported_foreign_resource_alias_keeps_a_portable_native_type() {
    use tembra::portable::Owned;
    use tembra_orb_native_fixture::codec::sink_reader;
    let _wrap: fn(Owned) -> Result<sink_reader::sink::ImportedDecoder, Owned> =
        sink_reader::sink::ImportedDecoder::try_from;
    let orb = sink_reader::orb_named(
        "sink-reader",
        1,
        sink_reader::Providers {
            sink: Provider::new("sink", 1),
        },
    )
    .unwrap();
    assert_eq!(orb.declaration().requires.len(), 1);
}

/// Signature metadata cannot relabel an attachment of another nominal resource type.
#[test]
fn a_mistyped_imported_resource_closes_without_leaving_an_anchored_claim() {
    use serde_json::Value;
    use std::cell::Cell;
    use tembra::{
        Code, Orb, Service,
        portable::{Issuable, Nominal, Payload},
    };
    struct Wrong(Rc<Cell<u32>>);
    impl Issuable for Wrong {
        fn nominal() -> Nominal {
            Nominal {
                interface: "example:protocol/codec@1.0.0".into(),
                name: "another-decoder".into(),
            }
        }
    }
    impl Drop for Wrong {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let closed = Rc::new(Cell::new(0));
    let declaration = provider::orb_named("codec", 1).unwrap();
    let metadata = &declaration.declaration().services[0];
    let mut service = Service::new("codec::example:protocol/codec@1.0.0", |_| Ok(()))
        .signature(metadata.signature.clone());
    for method in &metadata.methods {
        service = service.payload_method(method.clone(), |_, _, _: Payload| async {
            Ok(Payload::data(Value::Null))
        });
    }
    let seen = closed.clone();
    service = service.payload_method("open", move |_, _, _: Payload| {
        let closed = seen.clone();
        async move {
            let mut payload = Payload::data(Value::Null);
            let index = payload.issue(Wrong(closed));
            payload.encode(index)
        }
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("codec", 1).service(service),
        consumer::orb_named(
            "pipeline",
            1,
            consumer::Providers {
                codec: Provider::new("codec", 1),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let pipeline = Rc::new(
        host.get(
            "pipeline::example:codec-consumer/pipeline@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    pool.run_until(pipeline.ready()).unwrap();
    let claims = host.kernel().diagnostics().references;
    let called = pipeline.clone();
    let error = pool
        .run_until(host.root("host", move |context| {
            async move {
                called
                    .call_portable::<_, Value>(&context, "keep", (1_u64,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Invalid, "result")
    );
    assert_eq!(closed.get(), 1);
    assert_eq!(host.kernel().diagnostics().references, claims);
    pipeline.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
