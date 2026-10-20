use futures::FutureExt;
use serde_json::json;
use std::rc::Rc;
use tembra_core::*;

mod generated {
    include!("generated.rs");
}
mod shared {
    include!("shared.rs");
}
mod typed_counter {
    include!("typed_counter.rs");
}
mod remote {
    include!("remote.rs");
}

fn main() {
    let first = generated::contract().unwrap();
    assert!(Rc::ptr_eq(&first, &generated::contract().unwrap()));
    let address = Rc::as_ptr(&first) as usize;
    let fingerprint = first.fingerprint().to_owned();
    let (thread_address, thread_fingerprint, thread_source) = std::thread::spawn(|| {
        let descriptor = generated::contract().unwrap();
        assert!(Rc::ptr_eq(&descriptor, &generated::contract().unwrap()));
        (
            Rc::as_ptr(&descriptor) as usize,
            descriptor.fingerprint().to_owned(),
            serde_json::to_string(&descriptor).unwrap(),
        )
    })
    .join()
    .unwrap();
    assert_ne!(address, thread_address);
    assert_eq!(fingerprint, thread_fingerprint);
    assert_eq!(serde_json::to_string(&first).unwrap(), thread_source);
    assert!(Rc::ptr_eq(&first, &generated::contract().unwrap()));
    let main_descriptor = tembra::ContractToken::descriptor(&remote::variant_main::Token).unwrap();
    let other_descriptor =
        tembra::ContractToken::descriptor(&remote::variant_other::Token).unwrap();
    assert!(Rc::ptr_eq(&main_descriptor, &other_descriptor));
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("generated-test", json!({"kind": "host"})),
        driver.clone(),
    );
    let contract = generated::contract().unwrap();
    let typed_input: typed_counter::Input =
        serde_json::from_value(json!({"name": "visits"})).unwrap();
    assert_eq!(
        serde_json::to_value(typed_input).unwrap(),
        json!({"name": "visits"})
    );
    let typed_args: typed_counter::v_default_m_increment_args::Root =
        serde_json::from_value(json!({"by": 4})).unwrap();
    assert_eq!(serde_json::to_value(typed_args).unwrap(), json!({"by": 4}));
    let typed_result: typed_counter::v_default_m_increment_result::Root =
        serde_json::from_value(json!(5)).unwrap();
    assert_eq!(serde_json::to_value(typed_result).unwrap(), json!(5));
    let typed_state: typed_counter::variant_default::StateCount =
        serde_json::from_value(json!(6)).unwrap();
    assert_eq!(serde_json::to_value(typed_state).unwrap(), json!(6));
    assert_eq!(typed_counter::variant_default::VARIANT, "default");
    assert_eq!(typed_counter::variant_default::STATE_MEMBERS, ["count"]);
    assert_eq!(
        typed_counter::variant_default::METHOD_MEMBERS,
        ["increment", "read", "touch"]
    );
    assert_eq!(
        typed_counter::contract().unwrap().fingerprint(),
        typed_counter::CONTRACT_FINGERPRINT
    );
    let finite_calls = Rc::new(std::cell::Cell::new(0));
    let finite_effects = finite_calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let finite_effects = finite_effects.clone();
            Ok(Implementation::new()
                .json_method("add", |args, _| {
                    async move { Ok(Some(json!(args.unwrap()["by"].as_i64().unwrap() + 1))) }
                        .boxed_local()
                })
                .json_method("nullable", |args, _| {
                    async move {
                        assert_eq!(args, Some(serde_json::Value::Null));
                        Ok(Some(serde_json::Value::Null))
                    }
                    .boxed_local()
                })
                .json_method("finite", move |_, _| {
                    finite_effects.set(finite_effects.get() + 1);
                    async { Ok(Some(json!(true))) }.boxed_local()
                })
                .json_method("type", |args, _| {
                    async move {
                        assert_eq!(args, None);
                        Ok(Some(json!(99)))
                    }
                    .boxed_local()
                })
                .json_method("reference", |_, _| {
                    async { Ok(Some(json!(42))) }.boxed_local()
                })
                .json_method("check", |args, _| {
                    async move {
                        assert_eq!(args, Some(json!("ready")));
                        Ok(Some(json!(true)))
                    }
                    .boxed_local()
                })
                .json_method("echo", |args, _| async move { Ok(args) }.boxed_local())
                .json_method("defaults", |args, _| {
                    async move { Ok(Some(json!(args.unwrap().get("value").is_none()))) }
                        .boxed_local()
                })
                .json_method("presence", |args, _| async move { Ok(args) }.boxed_local()))
        }),
        Default::default(),
    )
    .unwrap();
    let reference = host.hold(contract, json!({}), Default::default()).unwrap();
    assert!(generated::variant_local::Consumer::new(reference.clone()).is_err());
    driver.run_until(reference.ready()).unwrap();
    let consumer = generated::variant_local::Consumer::new(reference.clone()).unwrap();
    assert_eq!(consumer.core().object_id(), reference.object_id());
    let remote_contract = remote::contract().unwrap();
    let invalid_remote_result = Rc::new(std::cell::Cell::new(false));
    let remote_result_mode = invalid_remote_result.clone();
    host.serve(
        remote_contract.clone(),
        Rc::new(move |_, _, _| {
            let remote_result_mode = remote_result_mode.clone();
            Ok(Implementation::new()
                .variant("main")
                .json_method("read", move |_, _| {
                    let invalid = remote_result_mode.get();
                    async move { Ok(Some(if invalid { json!("invalid") } else { json!(19) })) }
                        .boxed_local()
                }))
        }),
        Default::default(),
    )
    .unwrap();
    let remote_ref = driver
        .run_until(host.get(remote_contract, json!({}), Default::default()))
        .unwrap();
    let mismatch = generated::variant_local::Consumer::new(remote_ref.clone())
        .err()
        .unwrap();
    assert_eq!(mismatch.code, ErrorCode::ContractMismatch);
    assert_eq!(mismatch.reason, "consumer_contract");
    let mismatch = tembra::ContractToken::bind(&generated::GeneratedContract, remote_ref.clone())
        .err()
        .unwrap();
    assert_eq!(mismatch.code, ErrorCode::ContractMismatch);
    assert_eq!(mismatch.reason, "consumer_contract");
    assert!(remote::variant_other::Consumer::new(remote_ref.clone()).is_err());
    let remote = remote::variant_main::Consumer::new(remote_ref).unwrap();
    driver.run_until(async {
        assert_eq!(consumer.reference().await.unwrap(), 42);
        let result = consumer
            .add(serde_json::from_value(json!({"by": 4})).unwrap())
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), json!(5));
        let nullable = consumer.nullable(()).await.unwrap();
        assert_eq!(serde_json::to_value(nullable).unwrap(), json!(null));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = consumer.finite(Some(value)).await.unwrap_err();
            assert_eq!(error.reason, "argument_conversion");
            let mut args: generated::v_local_m_presence_args::Root =
                serde_json::from_value(json!({})).unwrap();
            args.float = generated::Presence::Present(Some(value));
            let error = consumer.presence(args).await.unwrap_err();
            assert_eq!(error.reason, "argument_conversion");
        }
        assert_eq!(finite_calls.get(), 0);
        assert!(consumer.finite(None).await.unwrap());
        assert!(consumer.finite(Some(1.5)).await.unwrap());
        assert_eq!(finite_calls.get(), 2);
        consumer.r#type().await.unwrap();
        assert!(
            consumer
                .check(serde_json::from_value(json!("ready")).unwrap())
                .await
                .unwrap()
        );
        let payload = json!({
            "type": "kept",
            "extra": {"nullable": null, "empty": []}
        });
        let echoed = consumer
            .echo(serde_json::from_value(payload.clone()).unwrap())
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(echoed).unwrap(), payload);
        assert!(
            consumer
                .defaults(serde_json::from_value(json!({})).unwrap())
                .await
                .unwrap()
        );
        for payload in [
            json!({}),
            json!({"arr": [], "map": {}, "nullable": null, "nested": {}}),
            json!({
                "arr": [1],
                "map": {"x": null},
                "nullable": "yes",
                "nested": {"items": [], "maybe": null}
            }),
        ] {
            let args = serde_json::from_value(payload.clone()).unwrap();
            let result = consumer.presence(args).await.unwrap();
            assert_eq!(serde_json::to_value(result).unwrap(), payload);
        }
        assert!(
            consumer
                .presence(serde_json::from_value(json!({"nonempty": []})).unwrap())
                .await
                .is_err()
        );
        assert!(serde_json::to_value(generated::Presence::<i64>::Missing).is_err());
        assert!(
            reference
                .call_json("add", Some(json!({"by": "wrong"})), None)
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(remote.read().await.unwrap()).unwrap(),
            json!(19)
        );
        assert_eq!(remote.read().await.unwrap(), 19);
        invalid_remote_result.set(true);
        assert!(remote.read().await.is_err());
        invalid_remote_result.set(false);
        assert_eq!(remote.read().await.unwrap(), 19);
        let facade = tembra::Host::from_core(host.clone());
        let inferred = facade
            .get(
                remote::variant_main::Token,
                serde_json::from_value(json!({})).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inferred.read().await.unwrap(), 19);
        assert_eq!(shared::variant_default::STATE_MEMBERS, ["count"]);
        assert_eq!(
            shared::contract().unwrap().fingerprint(),
            shared::CONTRACT_FINGERPRINT
        );
        host.close().await.unwrap();
    });
}
