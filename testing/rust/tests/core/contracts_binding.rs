use std::{cell::Cell, collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use serde_json::{Value, json};
use tembra_contracts::{Contract, Member, Placement, ValueSchema};
use tembra_core::{
    ErrorCode, GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions,
};

fn method(args: Option<Value>, result: Option<Value>) -> Member {
    Member::Method {
        args: args.map(ValueSchema::Portable),
        result: result.map(ValueSchema::Portable),
    }
}

fn local(name: &str, version: u64, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            version,
            json!({"type":"object","properties":{}}),
            members,
        )
        .unwrap(),
    )
}

#[test]
fn consumer_argument_bounds_are_checked_before_a_wider_provider_runs() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let consumer = local(
            "binding.consumer-bounds",
            1,
            BTreeMap::from([(
                "accept".into(),
                method(Some(json!({"type": "integer", "maximum": 1})), None),
            )]),
        );
        let provider = local(
            "binding.consumer-bounds",
            1,
            BTreeMap::from([(
                "accept".into(),
                method(Some(json!({"type": "integer"})), None),
            )]),
        );
        let host = Host::new(HostOptions::new("binding", json!("host")), driver.clone());
        let effects = Rc::new(Cell::new(0));
        let observed = effects.clone();
        host.serve(
            provider,
            Rc::new(move |_, _, _| {
                let observed = observed.clone();
                Ok(Implementation::new().json_method("accept", move |_, _| {
                    observed.set(observed.get() + 1);
                    async { Ok(None) }.boxed_local()
                }))
            }),
            Default::default(),
        )
        .unwrap();
        let reference = host
            .get(consumer, json!({}), Default::default())
            .await
            .unwrap();
        assert_eq!(
            reference
                .call_json("accept", Some(json!(2)), None)
                .await
                .unwrap_err()
                .reason,
            "args"
        );
        assert_eq!(effects.get(), 0);
        reference
            .call_json("accept", Some(json!(1)), None)
            .await
            .unwrap();
        assert_eq!(effects.get(), 1);
        host.close().await.unwrap();
    });
}

#[test]
fn additive_consumers_keep_their_token_and_cannot_call_unlisted_methods() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let old = local(
            "binding.additive",
            1,
            BTreeMap::from([("read".into(), method(None, Some(json!({"type":"number"}))))]),
        );
        let current = local(
            "binding.additive",
            1,
            BTreeMap::from([
                ("read".into(), method(None, Some(json!({"type":"integer"})))),
                ("added".into(), method(None, None)),
            ]),
        );
        let host = Host::new(HostOptions::new("binding", json!("host")), driver.clone());
        host.serve(
            current.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new()
                    .json_method("read", |args, _| {
                        async move {
                            assert!(args.is_none());
                            Ok(Some(json!(7)))
                        }
                        .boxed_local()
                    })
                    .json_method("added", |_, _| async { Ok(None) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let old_ref = host
            .get(old.clone(), json!({}), GetOptions::default())
            .await
            .unwrap();
        let new_ref = host
            .get(current.clone(), json!({}), GetOptions::default())
            .await
            .unwrap();
        assert_eq!(old_ref.object_id(), new_ref.object_id());
        assert_eq!(old_ref.contract().fingerprint(), old.fingerprint());
        assert_eq!(new_ref.contract().fingerprint(), current.fingerprint());
        assert_eq!(old_ref.shape().unwrap().members, ["read"]);
        assert_eq!(
            old_ref.call_json("read", None, None).await.unwrap(),
            Some(json!(7))
        );
        assert_eq!(
            old_ref
                .call_json("added", None, None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotInVariant
        );
        assert_eq!(new_ref.call_json("added", None, None).await.unwrap(), None);
        host.close().await.unwrap();
    });
}

#[test]
fn explicit_legacy_adapter_shares_object_and_maps_a_renamed_method() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let old = local(
            "binding.legacy",
            1,
            BTreeMap::from([(
                "old".into(),
                method(
                    Some(json!({"type":"string"})),
                    Some(json!({"type":"string"})),
                ),
            )]),
        );
        let current = local(
            "binding.legacy",
            2,
            BTreeMap::from([(
                "new".into(),
                method(
                    Some(json!({"type":"string"})),
                    Some(json!({"type":"string"})),
                ),
            )]),
        );
        let host = Host::new(HostOptions::new("binding", json!("host")), driver.clone());
        let adaptations = Rc::new(Cell::new(0));
        let count = adaptations.clone();
        let options = ServeOptions::default().with_adapter(
            1,
            Rc::new(move |current| {
                count.set(count.get() + 1);
                (*current).clone().alias_method("old", "new")
            }),
        );
        host.serve(
            current.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new().json_method("new", |args, _| {
                    async move {
                        Ok(Some(json!(format!(
                            "new:{}",
                            args.unwrap().as_str().unwrap()
                        ))))
                    }
                    .boxed_local()
                }))
            }),
            options,
        )
        .unwrap();
        let provider = host
            .get(current, json!({}), Default::default())
            .await
            .unwrap();
        let a = host
            .get(old.clone(), json!({}), Default::default())
            .await
            .unwrap();
        let b = host
            .get(old.clone(), json!({}), Default::default())
            .await
            .unwrap();
        assert_eq!(adaptations.get(), 1);
        assert_eq!(a.object_id(), provider.object_id());
        assert_eq!(
            a.call_json("old", Some(json!("value")), None)
                .await
                .unwrap(),
            Some(json!("new:value"))
        );
        assert_eq!(
            a.call_json("new", Some(json!("value")), None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotInVariant
        );
        a.release();
        b.release();
        let c = host.get(old, json!({}), Default::default()).await.unwrap();
        assert_eq!(
            adaptations.get(),
            2,
            "legacy view is released after its last consumer claim"
        );
        c.release();
        provider.release();
        host.close().await.unwrap();
    });
}

#[test]
fn remote_same_host_dispatch_validates_presence_args_results_and_void() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let contract = Rc::new(
            Contract::remote(
                "binding.remote",
                1,
                json!({"type":"object","properties":{}}),
                Placement::Named {
                    name: "service".into(),
                },
                BTreeMap::from([(
                    "default".into(),
                    BTreeMap::from([
                        (
                            "nullable".into(),
                            method(Some(json!({"type":"null"})), Some(json!({"type":"null"}))),
                        ),
                        ("read".into(), method(None, Some(json!({"type":"integer"})))),
                        ("bad".into(), method(None, Some(json!({"type":"integer"})))),
                        ("touch".into(), method(None, None)),
                    ]),
                )]),
            )
            .unwrap(),
        );
        let host = Host::new(HostOptions::new("service", json!("host")), driver.clone());
        host.serve(
            contract.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("nullable", |args, _| async move { Ok(args) }.boxed_local())
                    .json_method("read", |args, _| {
                        async move {
                            assert!(args.is_none());
                            Ok(Some(json!(4)))
                        }
                        .boxed_local()
                    })
                    .json_method("bad", |_, _| {
                        async { Ok(Some(json!("invalid result"))) }.boxed_local()
                    })
                    .json_method("touch", |_, _| {
                        async { Ok(Some(json!("discarded"))) }.boxed_local()
                    }))
            }),
            Default::default(),
        )
        .unwrap();
        let reference = host
            .get(contract, json!({}), Default::default())
            .await
            .unwrap();
        assert_eq!(
            reference.shape().unwrap().variant.as_deref(),
            Some("default")
        );
        assert_eq!(
            reference
                .call_json("nullable", Some(Value::Null), None)
                .await
                .unwrap(),
            Some(Value::Null)
        );
        assert_eq!(
            reference
                .call_json("nullable", None, None)
                .await
                .unwrap_err()
                .reason,
            "args"
        );
        assert_eq!(
            reference
                .call_json("nullable", Some(json!(1)), None)
                .await
                .unwrap_err()
                .reason,
            "args"
        );
        assert_eq!(
            reference
                .call_json("read", Some(Value::Null), None)
                .await
                .unwrap_err()
                .reason,
            "args"
        );
        assert_eq!(
            reference.call_json("read", None, None).await.unwrap(),
            Some(json!(4))
        );
        assert_eq!(
            reference
                .call_json("bad", None, None)
                .await
                .unwrap_err()
                .reason,
            "result"
        );
        assert_eq!(
            reference.call_json("touch", None, None).await.unwrap(),
            None
        );
        host.close().await.unwrap();
    });
}

#[test]
fn incompatible_binding_refuses_before_factory_or_object_activation() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let consumer = local(
            "binding.unsound",
            1,
            BTreeMap::from([(
                "read".into(),
                method(
                    None,
                    Some(json!({
                        "type": "object",
                        "properties": {"a": {"type": "string"}, "b": {"type": "number"}}
                    })),
                ),
            )]),
        );
        let provider = local(
            "binding.unsound",
            1,
            BTreeMap::from([(
                "read".into(),
                method(
                    None,
                    Some(json!({"type":"object","properties":{"a":{"type":"string"}}})),
                ),
            )]),
        );
        let host = Host::new(HostOptions::new("binding", json!("host")), driver.clone());
        let starts = Rc::new(Cell::new(0));
        let count = starts.clone();
        host.serve(
            provider,
            Rc::new(move |_, _, _| {
                count.set(count.get() + 1);
                Ok(Implementation::new().json_method("read", |_, _| {
                    async { Ok(Some(json!({"a":"ok","b":"wrong"}))) }.boxed_local()
                }))
            }),
            Default::default(),
        )
        .unwrap();
        assert_eq!(
            host.hold(consumer, json!({}), Default::default())
                .err()
                .unwrap()
                .code,
            ErrorCode::ContractMismatch
        );
        assert_eq!(starts.get(), 0);
        host.close().await.unwrap();
    });
}

#[test]
fn acquisition_defaults_and_placement_cannot_be_bridged_by_version_adapters() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let input = |default| {
            json!({
                "type": "object",
                "properties": {"n": {"type": "integer", "default": default}}
            })
        };
        let provider =
            Rc::new(Contract::local("binding.defaults", 2, input(2), BTreeMap::new()).unwrap());
        let consumer =
            Rc::new(Contract::local("binding.defaults", 1, input(1), BTreeMap::new()).unwrap());
        let host = Host::new(HostOptions::new("service", json!("host")), driver.clone());
        host.serve(
            provider,
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default().with_adapter(1, Rc::new(|current| Ok((*current).clone()))),
        )
        .unwrap();
        assert_eq!(
            host.hold(consumer, json!({}), Default::default())
                .err()
                .unwrap()
                .code,
            ErrorCode::ContractMismatch
        );
        let misplaced = Rc::new(
            Contract::remote(
                "binding.moved",
                1,
                json!({}),
                Placement::Named {
                    name: "other".into(),
                },
                BTreeMap::from([("default".into(), BTreeMap::new())]),
            )
            .unwrap(),
        );
        assert_eq!(
            host.serve(
                misplaced,
                Rc::new(|_, _, _| Ok(Implementation::new().variant("default"))),
                Default::default()
            )
            .unwrap_err()
            .reason,
            "placement"
        );
        host.close().await.unwrap();
    });
}

fn ambiguous_binding(name: &str) -> Rc<Contract> {
    local(
        name,
        1,
        BTreeMap::from([(
            "call".into(),
            method(
                Some(json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}},
                    {"type": "object", "properties": {"b": {"type": "integer"}}, "additionalProperties": true}
                ]})),
                None,
            ),
        )]),
    )
}

#[test]
fn ambiguous_self_binding_is_refused_before_service_admission() {
    let host = Host::new(
        HostOptions::new("binding", json!(null)),
        ManualDriver::new(),
    );
    let contract = ambiguous_binding("binding.ambiguous");
    let error = host
        .serve(
            contract,
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default(),
        )
        .unwrap_err();
    assert_eq!(error.reason, "ambiguous_normalization");
}

#[test]
fn duplicate_error_precedes_binding_validation_and_capture_drop_can_reenter() {
    struct Reenter {
        host: Host,
        dropped: Rc<Cell<bool>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            self.host
                .serve(
                    local("binding.from-drop", 1, BTreeMap::new()),
                    Rc::new(|_, _, _| Ok(Implementation::new())),
                    ServeOptions::default(),
                )
                .unwrap();
            self.dropped.set(true);
        }
    }
    let host = Host::new(
        HostOptions::new("binding", json!(null)),
        ManualDriver::new(),
    );
    host.serve(
        local("binding.order", 1, BTreeMap::new()),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default(),
    )
    .unwrap();
    let dropped = Rc::new(Cell::new(false));
    let capture = Reenter {
        host: host.clone(),
        dropped: dropped.clone(),
    };
    let error = host
        .serve(
            ambiguous_binding("binding.order"),
            Rc::new(move |_, _, _| {
                let _capture = &capture;
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )
        .unwrap_err();
    assert_eq!(error.reason, "duplicate");
    assert!(dropped.get());
}

#[test]
fn repeated_binding_success_does_not_skip_input_validation() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let make = || Rc::new(Contract::local(
            "binding.input-on-hit", 1,
            json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}),
            BTreeMap::new(),
        ).unwrap());
        let provider = make();
        let consumer = make();
        let host = Host::new(HostOptions::new("binding", json!(null)), driver.clone());
        host.serve(provider.clone(), Rc::new(|_, _, _| Ok(Implementation::new())), ServeOptions::default()).unwrap();
        for contract in [provider, consumer] {
            host.get(contract.clone(), json!({"id": 1}), GetOptions::default()).await.unwrap().release();
            let error = host.get(contract.clone(), json!({"id": "invalid"}), GetOptions::default()).await.err().unwrap();
            assert_eq!(error.reason, "validation");
        }
        host.close().await.unwrap();
    });
}

#[test]
fn service_drops_factory_captures_before_adapter_captures() {
    use std::cell::RefCell;
    struct Observe {
        name: &'static str,
        events: Rc<RefCell<Vec<&'static str>>>,
    }
    impl Drop for Observe {
        fn drop(&mut self) {
            self.events.borrow_mut().push(self.name);
        }
    }
    let events = Rc::new(RefCell::new(Vec::new()));
    let host = Host::new(
        HostOptions::new("binding", json!(null)),
        ManualDriver::new(),
    );
    let factory_capture = Observe {
        name: "factory",
        events: events.clone(),
    };
    let adapter_capture = Observe {
        name: "adapter",
        events: events.clone(),
    };
    host.serve(
        local("binding.drop-order", 1, BTreeMap::new()),
        Rc::new(move |_, _, _| {
            let _capture = &factory_capture;
            Ok(Implementation::new())
        }),
        ServeOptions::default().with_adapter(
            2,
            Rc::new(move |implementation| {
                let _capture = &adapter_capture;
                Ok((*implementation).clone())
            }),
        ),
    )
    .unwrap();
    drop(host);
    assert_eq!(*events.borrow(), ["factory", "adapter"]);
}
